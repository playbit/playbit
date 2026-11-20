//! A shared count for frontend aliases, pending results, backend resources and
//! asynchronous destruction. Clones retain one reservation, never a new charge.
use super::{HostError, HostResult};
use std::sync::{Arc, Mutex};

pub const MAX_FILES: usize = 512;
pub const MAX_LISTS: usize = 8;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    File,
    List,
}
#[derive(Default)]
struct Counts {
    files: usize,
    lists: usize,
}
#[derive(Default)]
pub struct Capacity {
    counts: Mutex<Counts>,
    release_wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}
struct Reservation {
    capacity: Arc<Capacity>,
    kind: Kind,
}
#[derive(Clone)]
pub struct Credit(Arc<Reservation>);
/// A call retains its source and reserves a possible result. A writer's close
/// reuses its source credit for recovery instead of reserving a second File.
#[derive(Clone, Default)]
pub struct Retention {
    pub source: Option<Credit>,
    pub result: Option<Credit>,
}
impl Capacity {
    /// One memory-only, coalescing owner wake, installed before root admission.
    /// The callback must not retain this Capacity or perform blocking work.
    pub fn on_release(&self, wake: Arc<dyn Fn() + Send + Sync>) -> HostResult<()> {
        let mut current = self.release_wake.lock().map_err(|_| HostError::Internal)?;
        if current.is_some() {
            return Err(HostError::Conflict);
        }
        *current = Some(wake);
        Ok(())
    }

    /// All-or-nothing reservation for one successful root enumeration. This is
    /// memory-only and bounded by the instance's engine capacity lease.
    pub fn reserve(self: &Arc<Self>, kind: Kind, count: usize) -> HostResult<Vec<Credit>> {
        let limit = match kind {
            Kind::File => MAX_FILES,
            Kind::List => MAX_LISTS,
        };
        let mut counts = self.counts.lock().map_err(|_| HostError::Internal)?;
        let used = match kind {
            Kind::File => &mut counts.files,
            Kind::List => &mut counts.lists,
        };
        assert!(*used <= limit, "resource reservations obey their declared ceiling");
        if count > limit - *used {
            return Err(HostError::LimitExceeded);
        }
        let credits = (0..count)
            .map(|_| Credit(Arc::new(Reservation { capacity: self.clone(), kind })))
            .collect();
        *used += count;
        Ok(credits)
    }
    pub fn one(self: &Arc<Self>, kind: Kind) -> HostResult<Credit> {
        Ok(self.reserve(kind, 1)?.pop().expect("one reserved resource"))
    }
}
impl Credit {
    pub fn belongs_to(&self, capacity: &Arc<Capacity>) -> bool {
        Arc::ptr_eq(&self.0.capacity, capacity)
    }
    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn kind(&self) -> Kind {
        self.0.kind
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut counts =
            self.capacity.counts.lock().expect("resource capacity lock is not poisoned");
        let used = match self.kind {
            Kind::File => &mut counts.files,
            Kind::List => &mut counts.lists,
        };
        assert!(*used > 0, "last resource owner releases exactly one reservation");
        *used -= 1;
        drop(counts);
        if let Some(wake) =
            self.capacity.release_wake.lock().expect("capacity wake lock is not poisoned").as_ref()
        {
            wake();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests check exact quota ownership and rollback.")]
mod tests {
    use super::*;
    #[test]
    fn enumeration_is_atomic_and_retirement_keeps_capacity_until_its_last_owner() {
        let capacity = Arc::new(Capacity::default());
        let roots = capacity.reserve(Kind::File, 256).unwrap();
        assert!(matches!(capacity.reserve(Kind::File, 257), Err(HostError::LimitExceeded)));
        let aliases = capacity.reserve(Kind::File, 256).unwrap();
        let retiring = aliases[0].clone();
        drop(aliases);
        let children = capacity.reserve(Kind::File, 255).unwrap();
        assert!(matches!(capacity.one(Kind::File), Err(HostError::LimitExceeded)));
        drop(retiring);
        let recovery = capacity.one(Kind::File).unwrap();
        let pending_close = recovery.clone();
        drop(recovery);
        assert!(matches!(capacity.one(Kind::File), Err(HostError::LimitExceeded)));
        drop(pending_close);
        drop(children);
        drop(roots);
        assert_eq!(capacity.reserve(Kind::File, MAX_FILES).unwrap().len(), MAX_FILES);
        let lists = capacity.reserve(Kind::List, MAX_LISTS).unwrap();
        assert!(matches!(capacity.one(Kind::List), Err(HostError::LimitExceeded)));
        drop(lists);
        assert!(capacity.one(Kind::List).is_ok());
    }
}
