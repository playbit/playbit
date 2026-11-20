//! Execution-generation File ownership. Root enumeration allocates aliases in
//! one memory-only transaction; every alias and backend resource shares a quota.
use super::capabilities::{Capabilities, FILE, LIST};
use super::{
    file_list::Cursor,
    host::{
        HostResult,
        files::capacity::{Capacity, Credit, Kind as CreditKind},
    },
};
use crate::capability::{Handle, Rights};
use playbit_api::abi::{
    files::{FileHandle, FileListHandle},
    syscall::Error as HostError,
};
use std::rc::Rc;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Directory,
    Reader,
    Writer,
    Recovery,
    List,
}
impl Kind {
    fn credit(self) -> CreditKind {
        if self == Self::List { CreditKind::List } else { CreditKind::File }
    }
}
#[derive(Clone)]
pub struct Resource {
    pub engine: Option<u64>,
    pub grant: [u8; 16],
    pub kind: Kind,
    pub credit: Credit,
    pub alias: bool,
    active: Arc<AtomicBool>,
    pub(crate) rights: Rights,
}
pub struct Entry {
    pub resource: Resource,
    pub cursor: Option<Cursor>,
}
/// A result owns its destination slot before I/O starts. Cloning a pending
/// operation retains the same credit and slot; insertion is allowed only once.
#[derive(Clone)]
pub struct Reservation {
    handle: Handle,
    kind: Kind,
    credit: Credit,
}
#[derive(Clone)]
pub struct Root {
    pub backing: u32,
    pub resource: Resource,
    pub writable: bool,
}
pub struct Table {
    generation: u32,
    next_grant: u64,
    capacity: Arc<Capacity>,
    roots: BTreeMap<[u8; 16], Root>,
}
impl Table {
    #[cfg(test)]
    pub fn new(generation: u32) -> Self {
        Self::with_capacity(generation, Arc::new(Capacity::default()))
    }
    pub fn with_capacity(generation: u32, capacity: Arc<Capacity>) -> Self {
        assert_ne!(generation, 0, "File handles belong to an admitted generation");
        Self { generation, next_grant: 1, capacity, roots: BTreeMap::new() }
    }
    pub fn capacity(&self) -> &Arc<Capacity> {
        &self.capacity
    }
    pub fn root_metadata(&self, volume: [u8; 16]) -> HostResult<([u8; 16], u32)> {
        let root = self.roots.get(&volume).ok_or(HostError::AccessDenied)?;
        Ok((root.resource.grant, root.backing))
    }
    pub fn owns_engine<P>(&self, caps: &Capabilities<P>, engine: u64) -> bool {
        self.roots.values().any(|root| root.resource.engine == Some(engine))
            || caps.files().any(|entry| entry.resource.engine == Some(engine))
    }
    /// Adopt an already-admitted helper root. No guest File is created here.
    pub fn adopt_root(&mut self, root: super::host::files::Root) -> HostResult<()> {
        if !matches!(
            root.backing,
            playbit_api::abi::data::VOLUME_PLAYBIT
                | playbit_api::abi::data::VOLUME_HOST_FS
                | playbit_api::abi::data::VOLUME_BUNDLE
        ) || root.engine == 0
            || root.replaces.is_some()
            || !root.credit.belongs_to(&self.capacity)
            || root.credit.kind() != CreditKind::File
            || root.volume == [0; 16]
            || root.grant == [0; 16]
            || self.roots.contains_key(&root.volume)
            || self.roots.values().any(|prior| {
                prior.resource.grant == root.grant
                    || prior.resource.engine == Some(root.engine)
                    || prior.resource.credit.same(&root.credit)
            })
        {
            return Err(HostError::InvalidArgument);
        }
        let credit = root.credit;
        self.roots.insert(
            root.volume,
            Root {
                backing: root.backing,
                resource: Resource {
                    engine: Some(root.engine),
                    grant: root.grant,
                    kind: Kind::Directory,
                    credit,
                    alias: false,
                    active: Arc::new(AtomicBool::new(true)),
                    rights: Rights(if root.writable { 3 } else { 1 }),
                },
                writable: root.writable,
            },
        );
        Ok(())
    }
    /// Embedding hosts without File operations still expose ordinary volume
    /// metadata. Their local root can be owned/closed, but cannot perform I/O.
    pub fn unsupported_root(&mut self, volume: [u8; 16], writable: bool) -> HostResult<()> {
        if self.roots.contains_key(&volume) {
            return Err(HostError::InvalidArgument);
        }
        let next = self.next_grant.checked_add(1).ok_or(HostError::LimitExceeded)?;
        let credit = self.capacity.one(CreditKind::File)?;
        let mut grant = [0; 16];
        grant[..4].copy_from_slice(b"none");
        grant[4..8].copy_from_slice(&self.generation.to_le_bytes());
        grant[8..].copy_from_slice(&self.next_grant.to_le_bytes());
        self.next_grant = next;
        self.roots.insert(
            volume,
            Root {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                resource: Resource {
                    engine: None,
                    grant,
                    kind: Kind::Directory,
                    credit,
                    alias: false,
                    active: Arc::new(AtomicBool::new(true)),
                    rights: Rights(if writable { 3 } else { 1 }),
                },
                writable,
            },
        );
        Ok(())
    }
    /// Preflight the complete next grant set. Failed admission leaves existing
    /// handles and authority intact; removed grants can never reactivate aliases.
    pub fn replace_roots<P>(
        &mut self,
        caps: &Capabilities<P>,
        volumes: &[super::host::AvailableVolume],
        admitted: &[super::host::files::Root],
        enabled: bool,
    ) -> HostResult<Vec<Resource>> {
        use super::host::VolumeRights;
        self.validate_roots(volumes, admitted)?;
        let mut candidate = Self::with_capacity(self.generation, self.capacity.clone());
        candidate.next_grant = self.next_grant;
        for volume in volumes {
            let previous = self.roots.get(&volume.id);
            if let Some(root) = admitted.iter().find(|root| root.volume == volume.id) {
                if let Some(previous) = previous.filter(|previous| {
                    previous.resource.engine == Some(root.engine)
                        && previous.resource.grant == root.grant
                        && previous.resource.credit.same(&root.credit)
                        && previous.writable == root.writable
                        && previous.backing == root.backing
                }) {
                    candidate.roots.insert(volume.id, previous.clone());
                } else {
                    if self.roots.values().any(|prior| {
                        prior.resource.engine == Some(root.engine)
                            || prior.resource.grant == root.grant
                            || (prior.resource.credit.same(&root.credit)
                                && prior.resource.engine != root.replaces)
                    }) {
                        return Err(HostError::InvalidArgument);
                    }
                    if caps.files().any(|entry| {
                        entry.resource.engine == Some(root.engine)
                            || entry.resource.credit.same(&root.credit)
                    }) {
                        return Err(HostError::InvalidArgument);
                    }
                    let mut root = root.clone();
                    root.replaces = None;
                    candidate.adopt_root(root)?;
                }
            } else {
                if enabled {
                    return Err(HostError::Unavailable);
                }
                let writable = volume.rights.contains(VolumeRights::WRITE);
                if let Some(previous) = previous.filter(|previous| {
                    previous.resource.engine.is_none() && previous.writable == writable
                }) {
                    candidate.roots.insert(volume.id, previous.clone());
                } else {
                    candidate.unsupported_root(volume.id, writable)?;
                }
            }
        }
        let mut retired = Vec::new();
        for (volume, root) in std::mem::take(&mut self.roots) {
            if candidate
                .roots
                .get(&volume)
                .is_none_or(|next| !Arc::ptr_eq(&root.resource.active, &next.resource.active))
            {
                root.resource.active.store(false, Ordering::Release);
                retired.push(root.resource);
            }
        }
        self.roots = candidate.roots;
        self.next_grant = candidate.next_grant;
        Ok(retired)
    }
    fn validate_roots(
        &self,
        volumes: &[super::host::AvailableVolume],
        admitted: &[super::host::files::Root],
    ) -> HostResult<()> {
        use super::host::VolumeRights;
        if admitted.len() > playbit_api::abi::syscall::MAX_VOLUMES {
            return Err(HostError::LimitExceeded);
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut grants = std::collections::BTreeSet::new();
        let mut engines = std::collections::BTreeSet::new();
        for root in admitted {
            let volume = volumes
                .iter()
                .find(|volume| volume.id == root.volume)
                .ok_or(HostError::InvalidArgument)?;
            if !matches!(
                root.backing,
                playbit_api::abi::data::VOLUME_PLAYBIT
                    | playbit_api::abi::data::VOLUME_HOST_FS
                    | playbit_api::abi::data::VOLUME_BUNDLE
            ) || root.backing != volume.backing
                || root.engine == 0
                || !root.credit.belongs_to(&self.capacity)
                || root.credit.kind() != CreditKind::File
                || root.grant == [0; 16]
                || !ids.insert(root.volume)
                || !grants.insert(root.grant)
                || !engines.insert(root.engine)
                || !volume.rights.contains(VolumeRights::READ)
                || root.writable && !volume.rights.contains(VolumeRights::WRITE)
            {
                return Err(HostError::InvalidArgument);
            }
        }
        let mut transfers = std::collections::BTreeSet::new();
        for root in admitted {
            if let Some(retired) = root.replaces
                && (engines.contains(&retired)
                    || !transfers.insert(retired)
                    || !self.roots.values().any(|prior| {
                        prior.resource.engine == Some(retired)
                            && prior.resource.credit.same(&root.credit)
                    }))
            {
                return Err(HostError::InvalidArgument);
            }
        }
        Ok(())
    }
    pub fn authorize_resource(&self, resource: &Resource, write: bool) -> HostResult<()> {
        if !resource.active.load(Ordering::Acquire) {
            return Err(HostError::AccessDenied);
        }
        if write && !resource.rights.contains(Rights::WRITE) {
            return Err(HostError::AccessDenied);
        }
        Ok(())
    }
    pub fn authorize(&self, grant: [u8; 16], write: bool) -> HostResult<()> {
        let root = self
            .roots
            .values()
            .find(|root| root.resource.grant == grant)
            .ok_or(HostError::AccessDenied)?;
        if write && !root.writable {
            return Err(HostError::AccessDenied);
        }
        Ok(())
    }
    /// The caller admits all output spans and capacities before this call.
    /// Invalid roots and quota/slot exhaustion leave the table unchanged.
    pub fn enumerate<P>(
        &mut self,
        caps: &mut Capabilities<P>,
        grants: &[[u8; 16]],
    ) -> HostResult<Vec<FileHandle>> {
        if grants.len() > playbit_api::abi::syscall::MAX_VOLUMES {
            return Err(HostError::LimitExceeded);
        }
        for grant in grants {
            if !self.roots.contains_key(grant) {
                return Err(HostError::AccessDenied);
            }
        }
        let credits = self.capacity.reserve(CreditKind::File, grants.len())?;
        let mut handles = Vec::with_capacity(grants.len());
        for grant in grants {
            let rights = self.roots[grant].resource.rights;
            match caps.reserve(FILE, rights) {
                Ok(handle) => handles.push(handle),
                Err(error) => {
                    for handle in handles {
                        caps.cancel(handle).expect("batch owns reservation");
                    }
                    return Err(error);
                }
            }
        }
        for ((grant, credit), handle) in grants.iter().zip(credits).zip(handles.iter().copied()) {
            let root = &self.roots[grant].resource;
            let resource = Resource {
                engine: root.engine,
                grant: root.grant,
                kind: Kind::Directory,
                credit,
                alias: true,
                active: root.active.clone(),
                rights: root.rights,
            };
            caps.insert_file(handle, Entry { resource, cursor: None })
                .expect("batch owns reservation");
        }
        Ok(handles.into_iter().map(|handle| FileHandle(handle.raw())).collect())
    }
    pub fn reserve<P>(
        &mut self,
        caps: &mut Capabilities<P>,
        kind: Kind,
    ) -> HostResult<Reservation> {
        let credit = self.capacity.one(kind.credit())?;
        let handle = caps.reserve(if kind == Kind::List { LIST } else { FILE }, Rights::NONE)?;
        Ok(Reservation { handle, kind, credit })
    }
    pub fn recovery<P>(
        &mut self,
        caps: &mut Capabilities<P>,
        writer: &Resource,
    ) -> HostResult<Reservation> {
        if writer.kind != Kind::Writer {
            return Err(HostError::InvalidArgument);
        }
        let handle = caps.reserve(FILE, Rights::NONE)?;
        Ok(Reservation { handle, kind: Kind::Recovery, credit: writer.credit.clone() })
    }
    pub fn insert<P>(
        &mut self,
        caps: &mut Capabilities<P>,
        reservation: &Reservation,
        engine: u64,
        grant: [u8; 16],
    ) -> HostResult<u64> {
        if engine == 0 {
            return Err(HostError::AccessDenied);
        }
        let root = self
            .roots
            .values()
            .find(|root| root.resource.grant == grant)
            .ok_or(HostError::AccessDenied)?;
        let resource = Resource {
            engine: Some(engine),
            grant,
            kind: reservation.kind,
            credit: reservation.credit.clone(),
            alias: false,
            active: root.resource.active.clone(),
            rights: root.resource.rights,
        };
        let cursor = (reservation.kind == Kind::List).then(Cursor::default);
        caps.insert_file(reservation.handle, Entry { resource, cursor })?;
        Ok(reservation.handle.raw())
    }
    pub fn file<P>(&self, caps: &Capabilities<P>, file: FileHandle) -> HostResult<Resource> {
        Ok(caps.file(Handle(file.0), false)?.resource.clone())
    }
    pub fn list<P>(&self, caps: &Capabilities<P>, list: FileListHandle) -> HostResult<Rc<Entry>> {
        caps.file(Handle(list.0), true)
    }
    pub fn remove_file<P>(
        &mut self,
        caps: &mut Capabilities<P>,
        file: FileHandle,
    ) -> HostResult<Resource> {
        Ok(caps.remove_file(Handle(file.0), false)?.resource.clone())
    }
    pub fn remove_list<P>(
        &mut self,
        caps: &mut Capabilities<P>,
        list: FileListHandle,
    ) -> HostResult<Rc<Entry>> {
        caps.remove_file(Handle(list.0), true)
    }
    pub fn revoke(&mut self, grant: [u8; 16]) -> Option<Resource> {
        self.roots.remove(&grant).map(|root| {
            root.resource.active.store(false, Ordering::Release);
            root.resource
        })
    }
    pub fn drain<P>(&mut self, caps: &mut Capabilities<P>) -> Vec<Resource> {
        let handles = caps
            .table
            .entries()
            .filter_map(|(id, kind, _, _)| {
                matches!(kind, FILE | LIST).then_some((id, kind == LIST))
            })
            .collect::<Vec<_>>();
        let mut resources = handles
            .into_iter()
            .map(|(id, list)| caps.remove_file(id, list).expect("enumerated File").resource.clone())
            .collect::<Vec<_>>();
        resources.extend(std::mem::take(&mut self.roots).into_values().map(|root| root.resource));
        resources
    }
}
impl Reservation {
    pub fn credit(&self) -> Credit {
        self.credit.clone()
    }
    pub fn handle(&self) -> u64 {
        self.handle.raw()
    }
    pub fn cancel<P>(&self, caps: &mut Capabilities<P>) {
        if !caps.table.contains(self.handle) {
            let _ = caps.cancel(self.handle);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests exercise handle ownership and atomic admission.")]
mod tests {
    use super::*;
    #[test]
    fn root_enumerations_are_independent_atomic_and_generation_scoped() {
        let mut caps = Capabilities::<()>::new();
        let mut table = Table::new(7);
        let grant = [1; 16];
        table
            .adopt_root(super::super::host::files::Root {
                replaces: None,
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                volume: grant,
                grant,
                engine: 42,
                writable: true,
                credit: table.capacity.one(CreditKind::File).unwrap(),
            })
            .unwrap();
        let first = table.enumerate(&mut caps, &[grant]).unwrap()[0];
        let second = table.enumerate(&mut caps, &[grant]).unwrap()[0];
        assert_ne!(first, second);
        drop(table.remove_file(&mut caps, first).unwrap());
        assert_eq!(table.file(&caps, second).unwrap().engine, Some(42));
        assert!(matches!(table.file(&caps, first), Err(HostError::StaleHandle)));
        assert!(matches!(
            table.file(&caps, FileHandle::new(8, second.slot())),
            Err(HostError::StaleHandle)
        ));
        let credits = table.capacity.reserve(CreditKind::File, 509).unwrap();
        assert!(matches!(
            table.enumerate(&mut caps, &[grant, grant]),
            Err(HostError::LimitExceeded)
        ));
        assert_eq!(caps.count(FILE) + caps.count(LIST), 1);
        assert!(matches!(
            table.enumerate(&mut caps, &[grant, [2; 16]]),
            Err(HostError::AccessDenied)
        ));
        let third = table.enumerate(&mut caps, &[grant]).unwrap()[0];
        assert_eq!(third.slot(), first.slot());
        assert_ne!(third.generation(), first.generation());
        drop(credits);
        let helper = table.revoke(grant).unwrap();
        assert!(matches!(table.authorize(grant, false), Err(HostError::AccessDenied)));
        assert!(table.remove_file(&mut caps, second).is_ok());
        assert!(table.remove_file(&mut caps, third).is_ok());
        drop(helper);
        assert!(table.capacity.reserve(CreditKind::File, 512).is_ok());
    }
    #[test]
    fn pending_results_and_recovery_share_the_complete_resource_ceiling() {
        let mut caps = Capabilities::<()>::new();
        let mut table = Table::new(7);
        let grant = [1; 16];
        table
            .adopt_root(super::super::host::files::Root {
                replaces: None,
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                volume: grant,
                grant,
                engine: 10,
                writable: true,
                credit: table.capacity.one(CreditKind::File).unwrap(),
            })
            .unwrap();
        let reserved = table.reserve(&mut caps, Kind::Writer).unwrap();
        let slot = table.insert(&mut caps, &reserved, 11, grant).unwrap();
        assert_eq!(table.insert(&mut caps, &reserved, 11, grant), Err(HostError::StaleHandle));
        drop(reserved);
        let writer = table.remove_file(&mut caps, FileHandle(slot)).unwrap();
        let full = table.capacity.reserve(CreditKind::File, 510).unwrap();
        let recovery = table.recovery(&mut caps, &writer).unwrap();
        drop(writer);
        assert!(matches!(table.reserve(&mut caps, Kind::Reader), Err(HostError::LimitExceeded)));
        let slot = table.insert(&mut caps, &recovery, 12, grant).unwrap();
        drop(recovery);
        assert_eq!(table.file(&caps, FileHandle(slot)).unwrap().kind, Kind::Recovery);
        assert!(matches!(table.list(&caps, FileListHandle(slot)), Err(HostError::StaleHandle)));
        drop(full);
        drop(table.drain(&mut caps));
        assert!(table.capacity.reserve(CreditKind::File, 512).is_ok());
    }
    #[test]
    fn replacing_grants_is_atomic_and_never_reactivates_old_aliases() {
        use super::super::host::{AvailableVolume, VolumeRights};
        let mut caps = Capabilities::<()>::new();
        let mut table = Table::new(8);
        let volume = AvailableVolume {
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            source_epoch: [0; 16],
            id: [1; 16],
            name: "Root".into(),
            tags: vec![],
            rights: VolumeRights::ALL,
            is_root: true,
            role: 0,
        };
        let first = super::super::host::files::Root {
            replaces: None,
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            volume: volume.id,
            grant: [2; 16],
            engine: 10,
            writable: true,
            credit: table.capacity.one(CreditKind::File).unwrap(),
        };
        table
            .replace_roots(&caps, std::slice::from_ref(&volume), std::slice::from_ref(&first), true)
            .unwrap();
        let alias = table.enumerate(&mut caps, &[volume.id]).unwrap()[0];
        let resource = table.file(&caps, alias).unwrap().clone();
        assert!(
            table
                .replace_roots(
                    &caps,
                    std::slice::from_ref(&volume),
                    std::slice::from_ref(&first),
                    true
                )
                .unwrap()
                .is_empty()
        );
        assert!(table.authorize_resource(&resource, true).is_ok());
        let mut replacement = super::super::host::files::Root {
            replaces: None,
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            volume: volume.id,
            grant: [3; 16],
            engine: 11,
            writable: true,
            credit: table.capacity.one(CreditKind::File).unwrap(),
        };
        replacement.credit = first.credit.clone();
        assert!(matches!(
            table.replace_roots(
                &caps,
                std::slice::from_ref(&volume),
                std::slice::from_ref(&replacement),
                true
            ),
            Err(HostError::InvalidArgument)
        ));
        assert!(table.authorize_resource(&resource, true).is_ok());
        replacement.credit = table.capacity.one(CreditKind::File).unwrap();
        let retired = table
            .replace_roots(&caps, std::slice::from_ref(&volume), &[replacement], true)
            .unwrap();
        assert_eq!(retired.len(), 1);
        assert_eq!(table.authorize_resource(&resource, false), Err(HostError::AccessDenied));
        // Even an erroneously recycled grant ID cannot revive its old active flag.
        let recycled = super::super::host::files::Root {
            replaces: None,
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            engine: 12,
            credit: table.capacity.one(CreditKind::File).unwrap(),
            ..first
        };
        table.replace_roots(&caps, &[volume], &[recycled], true).unwrap();
        assert_eq!(table.authorize_resource(&resource, false), Err(HostError::AccessDenied));
        assert!(table.remove_file(&mut caps, alias).is_ok());
    }
    #[test]
    fn helper_roots_require_distinct_pre_reserved_file_credits() {
        let mut caps = Capabilities::<()>::new();
        let mut table = Table::new(8);
        let credit = table.capacity.one(CreditKind::File).unwrap();
        let root = super::super::host::files::Root {
            replaces: None,
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            volume: [1; 16],
            grant: [2; 16],
            engine: 10,
            writable: true,
            credit,
        };
        table.adopt_root(root.clone()).unwrap();
        let forged = super::super::host::files::Root {
            replaces: None,
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            volume: [3; 16],
            grant: [4; 16],
            engine: 11,
            ..root.clone()
        };
        assert_eq!(table.adopt_root(forged), Err(HostError::InvalidArgument));
        let other = Arc::new(Capacity::default());
        let forged = super::super::host::files::Root {
            replaces: None,
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            volume: [3; 16],
            grant: [4; 16],
            engine: 11,
            credit: other.one(CreditKind::File).unwrap(),
            ..root
        };
        assert_eq!(table.adopt_root(forged), Err(HostError::InvalidArgument));
        let aliases = table.enumerate(&mut caps, &vec![[1; 16]; 256]).unwrap();
        let more = table.enumerate(&mut caps, &vec![[1; 16]; 255]).unwrap();
        assert!(matches!(table.capacity.one(CreditKind::File), Err(HostError::LimitExceeded)));
        drop(table.remove_file(&mut caps, aliases[0]).unwrap());
        let replacement = table.capacity.one(CreditKind::File).unwrap();
        assert!(matches!(table.enumerate(&mut caps, &[[1; 16]]), Err(HostError::LimitExceeded)));
        drop(replacement);
        assert!(table.enumerate(&mut caps, &[[1; 16]]).is_ok());
        assert_eq!(more.len(), 255);
    }
}
