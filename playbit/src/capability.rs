//! Local capability identities and bounded, reusable indexed storage.
//! Lookup is one index and an exact incarnation comparison. Slots never wrap
//! their generations. Backend tables share a process metadata budget; guest
//! tables have independent budgets. Payload ownership belongs to each domain.
use playbit_api::abi::syscall::Error;
use std::sync::{Arc, Mutex, OnceLock};

pub const GUEST_METADATA_BYTES: usize = 256 * 1024;
pub const BACKEND_METADATA_BYTES: usize = 1024 * 1024;
pub const MAX_GENERATION: u32 = i32::MAX as u32;
// Guest kinds are volume, file, list, operation, channel, audio and sound.
// Backend domains use the same tags locally for their resource/completion kinds.
const KIND_COUNT: usize = 7;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(transparent)]
/// Raw handle bits, including untrusted values awaiting validation.
/// Allocated identities use `[zero sign bit | 31-bit generation | 32-bit index]`.
pub struct Handle(pub u64);
impl Handle {
    /// Assemble raw parts; lookup validates parts decoded from guest input.
    pub const fn new(generation: u32, index: u32) -> Self {
        Self(((generation as u64) << 32) | index as u64)
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
    pub const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
    pub const fn index(self) -> u32 {
        self.0 as u32
    }
    pub const fn valid(self) -> bool {
        self.index() != 0 && self.generation() != 0 && self.generation() <= MAX_GENERATION
    }
}
const _: () = assert!(Handle::new(MAX_GENERATION, u32::MAX).raw() == i64::MAX as u64);
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(
    feature = "app-host",
    derive(serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct Rights(pub u32);
impl Rights {
    pub const NONE: Self = Self(0);
    pub const READ: Self = Self(1);
    pub const WRITE: Self = Self(2);
    pub const SCHEMA: Self = Self(4);
    pub const ALL: Self = Self(7);
    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }
}

pub struct Budget {
    limit: usize,
    used: Mutex<usize>,
}
impl Budget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self { limit, used: Mutex::new(0) })
    }
    pub fn guest() -> Arc<Self> {
        Self::new(GUEST_METADATA_BYTES)
    }
    pub fn backend() -> Arc<Self> {
        static SHARED: OnceLock<Arc<Budget>> = OnceLock::new();
        SHARED.get_or_init(|| Self::new(BACKEND_METADATA_BYTES)).clone()
    }
    fn claim(&self, bytes: usize) -> Result<(), Error> {
        let mut used = self.used.lock().expect("capability metadata budget lock is not poisoned");
        if bytes > self.limit - *used {
            return Err(Error::LimitExceeded);
        }
        *used += bytes;
        Ok(())
    }
    fn claim_growth(&self, old: usize, width: usize) -> Result<(usize, usize), Error> {
        let mut used = self.used.lock().expect("capability metadata budget lock is not poisoned");
        let available = (self.limit - *used) / width;
        let target =
            old.saturating_mul(2).max(16).min(old.saturating_add(available)).min(u32::MAX as usize);
        if target <= old {
            return Err(Error::LimitExceeded);
        }
        let charge = (target - old).checked_mul(width).ok_or(Error::LimitExceeded)?;
        *used += charge;
        Ok((target, charge))
    }
    fn release(&self, bytes: usize) {
        let mut used = self.used.lock().expect("capability metadata budget lock is not poisoned");
        assert!(*used >= bytes, "metadata release owns its charge");
        *used -= bytes;
    }
}
enum Value<T> {
    Free(u32),
    Reserved,
    Live(T),
    Retired,
}
struct Slot<T> {
    id: Handle,
    rights: Rights,
    kind: u16,
    value: Value<T>,
}
/// No references into this vector survive mutation. Reserved slots are invisible
/// to lookup; cancellation advances their incarnation just like ordinary close.
pub struct Table<T> {
    slots: Vec<Slot<T>>,
    free: u32,
    live: usize,
    live_by_kind: [usize; KIND_COUNT],
    reserved: usize,
    budget: Arc<Budget>,
}
impl<T> Table<T> {
    pub fn new(budget: Arc<Budget>) -> Self {
        const {
            assert!(std::mem::size_of::<Slot<T>>() <= 64, "store large payloads by reference");
        }
        Self {
            slots: Vec::new(),
            free: 0,
            live: 0,
            live_by_kind: [0; KIND_COUNT],
            reserved: 0,
            budget,
        }
    }
    pub fn len(&self) -> usize {
        self.live
    }
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }
    pub fn count(&self, kind: u16) -> usize {
        self.live_by_kind[Self::kind_index(kind)]
    }
    fn kind_index(kind: u16) -> usize {
        assert!((1..=KIND_COUNT as u16).contains(&kind), "known capability kind");
        kind as usize - 1
    }
    pub fn reserved(&self) -> usize {
        self.reserved
    }
    fn grow(&mut self) -> Result<(), Error> {
        let width = std::mem::size_of::<Slot<T>>();
        let old = self.slots.capacity();
        let (target, charge) = self.budget.claim_growth(old, width)?;
        let mut replacement = Vec::new();
        if replacement.try_reserve_exact(target).is_err() {
            self.budget.release(charge);
            return Err(Error::LimitExceeded);
        }
        let Some(extra) = (replacement.capacity() - target).checked_mul(width) else {
            self.budget.release(charge);
            return Err(Error::LimitExceeded);
        };
        if let Err(error) = self.budget.claim(extra) {
            self.budget.release(charge);
            return Err(error);
        }
        replacement.append(&mut self.slots);
        self.slots = replacement;
        Ok(())
    }
    pub fn reserve(&mut self, kind: u16, rights: Rights) -> Result<Handle, Error> {
        Self::kind_index(kind);
        let id = if self.free != 0 {
            let slot = &mut self.slots[self.free as usize - 1];
            let Value::Free(next) = slot.value else { unreachable!("free list owns free slots") };
            self.free = next;
            slot.kind = kind;
            slot.rights = rights;
            slot.value = Value::Reserved;
            slot.id
        } else {
            if self.slots.len() == u32::MAX as usize {
                return Err(Error::LimitExceeded);
            }
            if self.slots.len() == self.slots.capacity() {
                self.grow()?;
            }
            let id = Handle::new(1, self.slots.len() as u32 + 1);
            self.slots.push(Slot { id, rights, kind, value: Value::Reserved });
            id
        };
        assert!(id.valid(), "allocated handle is nonzero and fits a positive i64");
        self.reserved += 1;
        Ok(id)
    }
    pub fn insert(&mut self, id: Handle, value: T) -> Result<(), Error> {
        let slot = self.slot_mut(id).ok_or(Error::StaleHandle)?;
        if !matches!(slot.value, Value::Reserved) {
            return Err(Error::StaleHandle);
        }
        slot.value = Value::Live(value);
        let kind = Self::kind_index(slot.kind);
        self.live_by_kind[kind] += 1;
        self.reserved -= 1;
        self.live += 1;
        Ok(())
    }
    pub fn allocate(&mut self, kind: u16, rights: Rights, value: T) -> Result<Handle, Error> {
        self.insert_new(kind, rights, value).map(Handle)
    }
    pub fn update_rights(&mut self, id: Handle, kind: u16, rights: Rights) -> Result<(), Error> {
        self.authorize(id, kind, Rights::NONE)?;
        self.slot_mut(id).expect("authorized live capability").rights = rights;
        Ok(())
    }
    pub fn insert_new(&mut self, kind: u16, rights: Rights, value: T) -> Result<u64, Error> {
        let id = self.reserve(kind, rights)?;
        self.insert(id, value).expect("fresh reservation remains owned");
        Ok(id.raw())
    }
    fn slot(&self, id: Handle) -> Option<&Slot<T>> {
        if !id.valid() {
            return None;
        }
        self.slots.get(id.index() as usize - 1).filter(|slot| slot.id == id)
    }
    fn slot_mut(&mut self, id: Handle) -> Option<&mut Slot<T>> {
        if !id.valid() {
            return None;
        }
        self.slots.get_mut(id.index() as usize - 1).filter(|slot| slot.id == id)
    }
    fn authorize(&self, id: Handle, kind: u16, rights: Rights) -> Result<(), Error> {
        let slot = self.slot(id).ok_or(Error::StaleHandle)?;
        if slot.kind != kind || !matches!(slot.value, Value::Live(_)) {
            return Err(Error::StaleHandle);
        }
        if !slot.rights.contains(rights) {
            return Err(Error::AccessDenied);
        }
        Ok(())
    }
    pub fn get(&self, id: Handle, kind: u16, rights: Rights) -> Result<&T, Error> {
        let slot = self.slot(id).ok_or(Error::StaleHandle)?;
        let Value::Live(value) = &slot.value else { return Err(Error::StaleHandle) };
        if slot.kind != kind {
            return Err(Error::StaleHandle);
        }
        if !slot.rights.contains(rights) {
            return Err(Error::AccessDenied);
        }
        Ok(value)
    }
    pub fn get_mut(&mut self, id: Handle, kind: u16, rights: Rights) -> Result<&mut T, Error> {
        let slot = self.slot_mut(id).ok_or(Error::StaleHandle)?;
        let Value::Live(value) = &mut slot.value else { return Err(Error::StaleHandle) };
        if slot.kind != kind {
            return Err(Error::StaleHandle);
        }
        if !slot.rights.contains(rights) {
            return Err(Error::AccessDenied);
        }
        Ok(value)
    }
    pub fn lookup(&self, raw: &u64) -> Option<&T> {
        match &self.slot(Handle(*raw))?.value {
            Value::Live(value) => Some(value),
            _ => None,
        }
    }
    pub fn lookup_mut(&mut self, raw: &u64) -> Option<&mut T> {
        match &mut self.slot_mut(Handle(*raw))?.value {
            Value::Live(value) => Some(value),
            _ => None,
        }
    }
    pub fn contains(&self, id: Handle) -> bool {
        self.lookup(&id.raw()).is_some()
    }
    pub fn contains_key(&self, raw: &u64) -> bool {
        self.lookup(raw).is_some()
    }
    fn retire(&mut self, id: Handle) -> Option<Value<T>> {
        let next = self.free;
        let slot = self.slot_mut(id)?;
        if !matches!(slot.value, Value::Reserved | Value::Live(_)) {
            return None;
        }
        let value = std::mem::replace(&mut slot.value, Value::Retired);
        let kind = Self::kind_index(slot.kind);
        if id.generation() < MAX_GENERATION {
            slot.id = Handle::new(id.generation() + 1, id.index());
            slot.value = Value::Free(next);
            self.free = id.index();
        }
        match &value {
            Value::Live(_) => {
                self.live -= 1;
                self.live_by_kind[kind] -= 1;
            }
            Value::Reserved => self.reserved -= 1,
            _ => unreachable!(),
        }
        Some(value)
    }
    pub fn take(&mut self, raw: &u64) -> Option<T> {
        self.lookup(raw)?;
        match self.retire(Handle(*raw))? {
            Value::Live(value) => Some(value),
            _ => unreachable!(),
        }
    }
    pub fn remove(&mut self, id: Handle, kind: u16) -> Result<T, Error> {
        self.authorize(id, kind, Rights::NONE)?;
        Ok(self.take(&id.raw()).expect("authorized live capability"))
    }
    pub fn cancel(&mut self, id: Handle) -> Result<(), Error> {
        if !matches!(self.slot(id).map(|slot| &slot.value), Some(Value::Reserved)) {
            return Err(Error::StaleHandle);
        }
        self.retire(id).expect("owned reservation");
        Ok(())
    }
    pub fn entries(&self) -> impl Iterator<Item = (Handle, u16, Rights, &T)> {
        self.slots.iter().filter_map(|slot| match &slot.value {
            Value::Live(value) => Some((slot.id, slot.kind, slot.rights, value)),
            _ => None,
        })
    }
    pub fn iter(&self) -> impl Iterator<Item = (u64, &T)> {
        self.entries().map(|(id, _, _, value)| (id.raw(), value))
    }
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (u64, &mut T)> {
        self.slots.iter_mut().filter_map(|slot| match &mut slot.value {
            Value::Live(value) => Some((slot.id.raw(), value)),
            _ => None,
        })
    }
    pub fn retain(&mut self, mut keep: impl FnMut(u64, &mut T) -> bool) {
        let ids = self.keys().collect::<Vec<_>>();
        for id in ids {
            if !keep(id, self.lookup_mut(&id).expect("enumerated live capability")) {
                self.take(&id);
            }
        }
    }
    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.entries().map(|(_, _, _, value)| value)
    }
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.slots.iter_mut().filter_map(|slot| match &mut slot.value {
            Value::Live(value) => Some(value),
            _ => None,
        })
    }
    pub fn keys(&self) -> impl Iterator<Item = u64> + '_ {
        self.entries().map(|(id, _, _, _)| id.raw())
    }
    pub fn drain(&mut self) -> Vec<T> {
        let ids = self.keys().collect::<Vec<_>>();
        ids.into_iter().map(|id| self.take(&id).expect("enumerated live capability")).collect()
    }
}
impl<T> Drop for Table<T> {
    fn drop(&mut self) {
        let slots = std::mem::take(&mut self.slots);
        let bytes = slots.capacity() * std::mem::size_of::<Slot<T>>();
        drop(slots);
        self.budget.release(bytes);
    }
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self::new(Budget::backend())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests exercise allocation and stale identity transitions.")]
mod tests {
    use super::*;
    #[test]
    fn kinds_rights_reservations_and_reuse_share_one_namespace() {
        // Reuse a slot across kinds; old and unpublished identities must fail.
        let mut table = Table::new(Budget::guest());
        let first = table.reserve(1, Rights::READ).unwrap();
        assert!(table.lookup(&first.raw()).is_none());
        assert_eq!(table.count(1), 0);
        table.insert(first, 7u64).unwrap();
        assert_eq!(table.count(1), 1);
        assert_eq!(table.get(first, 1, Rights::READ), Ok(&7));
        assert_eq!(table.get(first, 1, Rights::WRITE), Err(Error::AccessDenied));
        assert_eq!(table.get(first, 2, Rights::NONE), Err(Error::StaleHandle));
        assert_eq!(table.remove(first, 1), Ok(7));
        assert_eq!(table.count(1), 0);
        let second = table.allocate(2, Rights::WRITE, 9).unwrap();
        assert_eq!(first.index(), second.index());
        assert_eq!(second.generation(), first.generation() + 1);
        assert_eq!(table.get(first, 1, Rights::NONE), Err(Error::StaleHandle));
        for raw in [0, 1, u64::MAX, 1 << 63] {
            assert!(table.lookup(&raw).is_none());
        }
    }
    #[test]
    fn last_generation_retires_without_reducing_live_quota() {
        // Reach the counter boundary directly, then require a different slot.
        let mut table = Table::new(Budget::guest());
        let first = table.allocate(1, Rights::NONE, 7u64).unwrap();
        let last = Handle::new(MAX_GENERATION, first.index());
        table.slots[0].id = last;
        assert_eq!(table.remove(last, 1), Ok(7));
        let next = table.allocate(1, Rights::NONE, 9).unwrap();
        assert_ne!(next.index(), last.index());
        assert!(table.lookup(&last.raw()).is_none());
        assert!(next.raw() <= i64::MAX as u64);
    }
    #[test]
    fn cancellation_and_ordinary_churn_do_not_grow_storage() {
        // Repeatedly cancel and close the same physical cell without consuming IDs.
        let mut table = Table::new(Budget::guest());
        for value in 0..10000u64 {
            let reserved = table.reserve(1, Rights::NONE).unwrap();
            table.cancel(reserved).unwrap();
            let live = table.allocate(2, Rights::NONE, value).unwrap();
            assert_eq!(table.take(&live.raw()), Some(value));
            assert!(table.lookup(&reserved.raw()).is_none());
        }
        assert_eq!(table.slots.len(), 1);
        assert_eq!(table.reserved(), 0);
        assert!(table.is_empty());
    }
    #[test]
    fn metadata_budget_is_aggregate_and_failed_growth_preserves_ownership() {
        // Two domains compete for one bounded allocation; dropping it releases credit.
        let budget = Budget::new(16 * std::mem::size_of::<Slot<u64>>());
        let mut first = Table::<u64>::new(budget.clone());
        let mut second = Table::<u64>::new(budget.clone());
        let id = first.allocate(1, Rights::READ, 1).unwrap();
        assert_eq!(second.reserve(1, Rights::READ), Err(Error::LimitExceeded));
        assert_eq!(first.get(id, 1, Rights::READ), Ok(&1));
        drop(first);
        assert!(second.allocate(1, Rights::READ, 2).is_ok());
    }
    #[test]
    fn numeric_identity_is_local_to_its_table() {
        // Equal numbers in independent guest namespaces convey their own objects.
        let mut a = Table::new(Budget::guest());
        let mut b = Table::new(Budget::guest());
        let first = a.allocate(1, Rights::READ, 1u64).unwrap();
        let second = b.allocate(1, Rights::READ, 2u64).unwrap();
        assert_eq!(first, second);
        assert_eq!(a.get(first, 1, Rights::READ), Ok(&1));
        assert_eq!(b.get(first, 1, Rights::READ), Ok(&2));
    }
}
