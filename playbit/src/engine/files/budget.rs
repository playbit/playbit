//! Credits follow retained resources and accepted operations through cleanup,
//! including pending results and work whose caller has disappeared.
use super::resource::Kind;
use crate::{runtime::host::HostResult, store::Id};
use playbit_api::abi::syscall::Error;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
};

#[derive(Clone, Copy, Default)]
struct Usage {
    files: usize,
    lists: usize,
    writers: usize,
    calls: usize,
    bytes: usize,
}
impl Usage {
    fn add(self, other: Self) -> Self {
        Self {
            files: self.files + other.files,
            lists: self.lists + other.lists,
            writers: self.writers + other.writers,
            calls: self.calls + other.calls,
            bytes: self.bytes + other.bytes,
        }
    }
    fn subtract(&mut self, other: Self) {
        assert!(
            self.files >= other.files
                && self.lists >= other.lists
                && self.writers >= other.writers
                && self.calls >= other.calls
                && self.bytes >= other.bytes
        );
        self.files -= other.files;
        self.lists -= other.lists;
        self.writers -= other.writers;
        self.calls -= other.calls;
        self.bytes -= other.bytes;
    }
    fn fits(self, global: bool) -> bool {
        self.files <= if global { 4096 } else { 512 }
            && self.lists <= if global { 64 } else { 8 }
            && self.writers <= if global { 16 } else { 8 }
            && self.calls <= if global { 1024 } else { 64 }
            && self.bytes <= if global { 128 * 1024 * 1024 } else { 16 * 1024 * 1024 }
    }
    fn empty(self) -> bool {
        self.files == 0 && self.lists == 0 && self.calls == 0
    }
}
#[derive(Default)]
struct State {
    total: Usage,
    owners: BTreeMap<Id, Usage>,
    leases: BTreeMap<Id, Weak<Lease>>,
}
#[derive(Default)]
pub struct Budget(Mutex<State>);
pub struct Credit {
    budget: Arc<Budget>,
    owner: Id,
    charge: Usage,
    global_charge: Usage,
    _lease: Option<Arc<Lease>>,
}
/// One context reserves its entire synchronous root/child handle allowance.
/// Resource credits keep the lease alive through worker-side destruction.
pub struct Lease {
    budget: Arc<Budget>,
    owner: Id,
}
impl Budget {
    pub fn lease(self: &Arc<Self>, owner: Id) -> HostResult<Arc<Lease>> {
        let mut state = self.0.lock().map_err(|_| Error::Internal)?;
        if let Some(lease) = state.leases.get(&owner) {
            // A zero strong count can precede Lease::drop acquiring this lock.
            // Its ledger entry remains owned until that destructor removes it.
            return lease.upgrade().ok_or(Error::Busy);
        }
        // Lease installation precedes every resource admission for that owner.
        // Existing nonleased resources cannot be charged to a different ledger.
        if state.owners.get(&owner).is_some_and(|usage| usage.files != 0 || usage.lists != 0) {
            return Err(Error::Conflict);
        }
        let total = state.total.add(Usage { files: 512, ..Usage::default() });
        if !total.fits(true) {
            return Err(Error::LimitExceeded);
        }
        let lease = Arc::new(Lease { budget: self.clone(), owner });
        state.total = total;
        state.leases.insert(owner, Arc::downgrade(&lease));
        Ok(lease)
    }
    pub fn retained(&self, owner: Id) -> bool {
        let state = self.0.lock().expect("file resource credit lock is not poisoned");
        state.owners.contains_key(&owner) || state.leases.contains_key(&owner)
    }
    pub fn resource(self: &Arc<Self>, owner: Id, kind: Kind) -> HostResult<Arc<Credit>> {
        let charge = Usage {
            files: usize::from(kind != Kind::List),
            lists: usize::from(kind == Kind::List),
            writers: usize::from(matches!(kind, Kind::Writer | Kind::Recovery)),
            ..Usage::default()
        };
        self.reserve(owner, charge)
    }
    pub fn operation(self: &Arc<Self>, owner: Id, bytes: usize) -> HostResult<Arc<Credit>> {
        if bytes > 16 * 1024 * 1024 {
            return Err(Error::LimitExceeded);
        }
        self.reserve(owner, Usage { calls: 1, bytes, ..Usage::default() })
    }
    fn reserve(self: &Arc<Self>, owner: Id, charge: Usage) -> HostResult<Arc<Credit>> {
        let mut state = self.0.lock().map_err(|_| Error::Internal)?;
        let usage = state.owners.get(&owner).copied().unwrap_or_default().add(charge);
        let lease =
            state.leases.get(&owner).map(|lease| lease.upgrade().ok_or(Error::Busy)).transpose()?;
        let global_charge =
            Usage { files: if lease.is_some() { 0 } else { charge.files }, ..charge };
        let total = state.total.add(global_charge);
        if !usage.fits(false) || !total.fits(true) {
            drop(state);
            return Err(Error::LimitExceeded);
        }
        state.owners.insert(owner, usage);
        state.total = total;
        Ok(Arc::new(Credit { budget: self.clone(), owner, charge, global_charge, _lease: lease }))
    }
}
impl Drop for Credit {
    fn drop(&mut self) {
        let mut state = self.budget.0.lock().expect("file resource credit lock is not poisoned");
        state.total.subtract(self.global_charge);
        let usage =
            state.owners.get_mut(&self.owner).expect("credit retains its owner's accounting");
        usage.subtract(self.charge);
        if usage.empty() {
            state.owners.remove(&self.owner);
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.budget.0.lock().expect("file resource credit lock is not poisoned");
        assert!(
            state.owners.get(&self.owner).is_none_or(|usage| usage.files == 0 && usage.lists == 0),
            "resources retain their lease until destruction"
        );
        state.total.subtract(Usage { files: 512, ..Usage::default() });
        state.leases.remove(&self.owner);
    }
}
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests exercise exact admission ceilings and retained credit."
)]
mod tests {
    use super::*;
    #[test]
    fn a_lease_waiting_for_destruction_cannot_be_replaced_or_bypassed() {
        let budget = Arc::new(Budget::default());
        let owner = Id::from_bytes([1; 16]);
        // The last Arc has reached zero while its destructor waits for the
        // ledger mutex. It still owns the reservation and removal of this key.
        {
            let mut state = budget.0.lock().unwrap();
            state.leases.insert(owner, Weak::new());
            state.total.files = 512;
        }
        assert!(matches!(budget.lease(owner), Err(Error::Busy)));
        assert!(matches!(budget.resource(owner, Kind::Reader), Err(Error::Busy)));
        assert!(budget.retained(owner));
        assert_eq!(budget.0.lock().unwrap().total.files, 512);
    }
    #[test]
    fn synchronous_context_leases_bound_the_aggregate_through_last_resource_release() {
        let budget = Arc::new(Budget::default());
        let owners: Vec<_> = (1..=9).map(|id| Id::from_bytes([id; 16])).collect();
        let mut leases =
            (0..8).map(|index| budget.lease(owners[index]).unwrap()).collect::<Vec<_>>();
        assert!(matches!(budget.lease(owners[8]), Err(Error::LimitExceeded)));
        let files =
            (0..512).map(|_| budget.resource(owners[0], Kind::Reader).unwrap()).collect::<Vec<_>>();
        assert!(matches!(budget.resource(owners[0], Kind::Reader), Err(Error::LimitExceeded)));
        assert!(matches!(budget.resource(owners[8], Kind::Reader), Err(Error::LimitExceeded)));
        assert_eq!(budget.0.lock().unwrap().total.files, 4096);
        drop(leases.remove(0));
        assert!(budget.retained(owners[0]));
        assert!(matches!(budget.lease(owners[8]), Err(Error::LimitExceeded)));
        drop(files);
        assert!(!budget.retained(owners[0]));
        let replacement = budget.lease(owners[8]).unwrap();
        assert_eq!(budget.0.lock().unwrap().total.files, 4096);
        drop(replacement);
        drop(leases);
        assert_eq!(budget.0.lock().unwrap().total.files, 0);
    }
    #[test]
    fn limits_cover_pending_resources_and_recovery_reuses_writer_credit() {
        let budget = Arc::new(Budget::default());
        let a = Id::from_bytes([1; 16]);
        let b = Id::from_bytes([2; 16]);
        let mut writers =
            (0..8).map(|_| budget.resource(a, Kind::Writer).unwrap()).collect::<Vec<_>>();
        assert!(matches!(budget.resource(a, Kind::Writer), Err(Error::LimitExceeded)));
        let recovery = writers.pop().unwrap();
        let pending_close = recovery.clone();
        drop(recovery);
        assert!(matches!(budget.resource(a, Kind::Writer), Err(Error::LimitExceeded)));
        let others = (0..8).map(|_| budget.resource(b, Kind::Writer).unwrap()).collect::<Vec<_>>();
        assert!(matches!(
            budget.resource(Id::from_bytes([3; 16]), Kind::Writer),
            Err(Error::LimitExceeded)
        ));
        drop(pending_close);
        assert!(budget.resource(a, Kind::Writer).is_ok());
        drop(writers);
        drop(others);
        let calls = (0..64).map(|_| budget.operation(a, 256 * 1024).unwrap()).collect::<Vec<_>>();
        assert!(matches!(budget.operation(a, 1), Err(Error::LimitExceeded)));
        drop(calls);
        assert!(budget.0.lock().unwrap().owners.is_empty());
    }
}
