//! Browser execution-owned Files. Root helpers and guest aliases share the
//! runtime's resource ceiling; metadata refresh never allocates guest handles.
mod destination;
mod objects;
mod pending;
mod readers;
mod writer;
use super::*;
use crate::capability::{Budget, Handle, Rights, Table as HandleTable};
use crate::runtime::{
    file_handles::Table,
    host::{AvailableVolume, VolumeRights, files as runtime},
};
use playbit_api::abi::{data as abi, files::FileHandle};
use std::sync::Arc;

struct Root {
    volume: Id,
    admitted: runtime::Root,
    metadata: AvailableVolume,
    slot: usize,
    handle: Handle,
}
pub(super) struct Files {
    pub(super) bundle: Option<(crate::package::Package, [u8; 16])>,
    generation: u32,
    pub(super) revision: u64,
    pub(super) table: Table,
    roots: Vec<Root>,
    objects: HandleTable<Rc<objects::Object>>,
    stages: Rc<Cell<usize>>,
    staged: BTreeMap<u64, Rc<writer::StageCredit>>,
    readers: Rc<RefCell<readers::Budget>>,
    pending_bytes: usize,
    next_pending: u64,
    pending: HandleTable<Box<pending::Pending>>,
}
impl Files {
    pub(super) fn ready_for_bundle(&self) -> bool {
        self.roots.is_empty() && self.pending.is_empty()
    }
    pub(super) fn new(cache: &Rc<RefCell<blob_cache::Cache>>) -> Self {
        Self {
            bundle: None,
            generation: 1,
            revision: 1,
            table: Table::with_capacity(1, Arc::default()),
            roots: Vec::new(),
            objects: HandleTable::new(Budget::backend()),
            stages: Rc::new(Cell::new(0)),
            staged: BTreeMap::new(),
            readers: Rc::new(RefCell::new(readers::Budget::new(cache))),
            pending_bytes: 0,
            next_pending: 1,
            pending: HandleTable::new(Budget::backend()),
        }
    }
    pub(super) fn admit_reset(&self) -> Result<()> {
        if !self.pending.is_empty() {
            return Err(Error::Busy);
        }
        self.generation.checked_add(1).ok_or(Error::LimitExceeded)?;
        self.revision.checked_add(1).ok_or(Error::LimitExceeded)?;
        Ok(())
    }
    pub(super) fn reset(
        &mut self,
        capabilities: &mut crate::runtime::capabilities::Capabilities<work::Operation>,
    ) -> Result<()> {
        if !self.pending.is_empty() {
            return Err(Error::Busy);
        }
        let generation = self.generation.checked_add(1).ok_or(Error::LimitExceeded)?;
        let revision = self.revision.checked_add(1).ok_or(Error::LimitExceeded)?;
        self.bundle = None;
        self.roots.clear();
        drop(self.objects.drain());
        drop(self.table.drain(capabilities));
        self.table = Table::with_capacity(generation, Arc::default());
        self.generation = generation;
        self.revision = revision;
        Ok(())
    }
    /// Withdrawal invalidates aliases immediately, even when metadata refresh
    /// coalesces multiple consent notices. Revoked handles remain disposable.
    pub(super) fn retain_selection(
        &mut self,
        selections: &[PersonalVolumeSelection],
    ) -> Result<()> {
        let retained = |root: &Root| {
            root.metadata.backing == abi::VOLUME_BUNDLE
                || root.slot == 0
                || root.metadata.role != 0
                || selections.iter().any(|grant| {
                    grant.volume == root.volume
                        && grant.write == root.admitted.writable
                        && *grant.incarnation.as_bytes() == root.metadata.source_epoch
                })
        };
        if self.roots.iter().all(retained) {
            return Ok(());
        }
        let revision = self.revision.checked_add(1).ok_or(Error::LimitExceeded)?;
        self.roots.retain(|root| {
            if retained(root) {
                true
            } else {
                drop(self.table.revoke(root.metadata.id));
                self.objects.take(&root.admitted.engine);
                work::capabilities(|capabilities| {
                    capabilities
                        .table
                        .remove(root.handle, crate::runtime::capabilities::VOLUME)
                        .map(|_| ())
                })
                .expect("retained root owns its volume capability");
                false
            }
        });
        self.revision = revision;
        Ok(())
    }
    /// Runs outside guest dispatch on the existing runtime worker. Authorization
    /// may read local indexes; enumerate below uses only this admitted state.
    pub(super) fn refresh(&mut self, host: &Host) -> Result<()> {
        let retained = self.objects.keys().collect::<std::collections::BTreeSet<_>>();
        let result = self.refresh_inner(host);
        if result.is_err() {
            self.objects.retain(|id, _| retained.contains(&id));
        }
        result
    }
    fn refresh_inner(&mut self, host: &Host) -> Result<()> {
        let mut selected = Vec::new();
        for metadata in host.automatic_metadata() {
            selected.push((0, metadata));
        }
        for (slot, volume) in host.selected_volumes() {
            if host.has_automatic() && host.automatic_role(volume.id).is_some() {
                if slot == 0 || host.user_volume == Some(volume.id) {
                    continue;
                }
                return Err(Error::Conflict);
            }
            selected.push((slot, root_metadata(host, slot, &volume)?));
        }
        if selected.len() + usize::from(self.bundle.is_some())
            > playbit_api::abi::syscall::MAX_VOLUMES
        {
            return Err(Error::LimitExceeded);
        }
        let mut roots = Vec::with_capacity(selected.len());
        let mut changed = selected.len() + usize::from(self.bundle.is_some()) != self.roots.len();
        for (slot, metadata) in selected {
            let id = Id::from_bytes(metadata.id);
            let writable = metadata.rights.contains(VolumeRights::WRITE);
            let prior = self.roots.iter().find(|root| root.volume == id);
            let handle = prior.map_or(Handle::default(), |root| root.handle);
            let unchanged = prior.filter(|root| {
                root.admitted.writable == writable
                    && root.metadata.source_epoch == metadata.source_epoch
            });
            let admitted = if let Some(prior) = unchanged {
                changed |= prior.metadata != metadata || prior.slot != slot;
                prior.admitted.clone()
            } else {
                changed = true;
                let replaces = prior.map(|root| root.admitted.engine);
                let credit = prior.map(|root| root.admitted.credit.clone());
                self.admit_root(id, &metadata, replaces, credit)?
            };
            roots.push(Root { volume: id, admitted, metadata, slot, handle });
        }
        if let Some((_, id)) = &self.bundle {
            let bundle_id = *id;
            let (root, added) = self.bundle_root(bundle_id)?;
            changed |= added;
            roots.push(root);
        }
        if !changed && roots.len() == self.roots.len() {
            return Ok(());
        }
        let revision = self.revision.checked_add(1).ok_or(Error::LimitExceeded)?;
        self.admit_roots(&mut roots)?;
        // A replacement receipt applies only to the transition above.
        for root in &mut roots {
            root.admitted.replaces = None;
        }
        for prior in &self.roots {
            if !roots.iter().any(|root| root.admitted.engine == prior.admitted.engine) {
                self.objects.take(&prior.admitted.engine);
            }
        }
        self.roots = roots;
        self.revision = revision;
        Ok(())
    }
    fn admit_root(
        &mut self,
        volume: Id,
        metadata: &AvailableVolume,
        replaces: Option<u64>,
        credit: Option<runtime::capacity::Credit>,
    ) -> Result<runtime::Root> {
        let object = objects::Object::Directory(objects::Directory { volume, parent: None });
        let engine = self.objects.allocate(1, Rights::ALL, Rc::new(object))?.raw();
        let mut grant = [0; 16];
        grant[..4].copy_from_slice(b"webf");
        grant[4..8].copy_from_slice(&self.generation.to_le_bytes());
        grant[8..].copy_from_slice(&engine.to_le_bytes());
        Ok(runtime::Root {
            replaces,
            backing: abi::VOLUME_PLAYBIT,
            volume: metadata.id,
            grant,
            engine,
            writable: metadata.rights.contains(VolumeRights::WRITE),
            credit: match credit {
                Some(credit) => credit,
                None => self.table.capacity().one(runtime::capacity::Kind::File)?,
            },
        })
    }
    fn admit_roots(&mut self, roots: &mut [Root]) -> Result<()> {
        let metadata = roots.iter().map(|root| root.metadata.clone()).collect::<Vec<_>>();
        let admitted = roots.iter().map(|root| root.admitted.clone()).collect::<Vec<_>>();
        work::capabilities(|capabilities| {
            let mut created = Vec::new();
            for root in roots.iter_mut() {
                if root.handle.raw() == 0 {
                    match capabilities.add_volume(root.metadata.clone()) {
                        Ok(handle) => {
                            root.handle = handle;
                            created.push(handle);
                        }
                        Err(error) => {
                            for handle in created {
                                capabilities
                                    .table
                                    .remove(handle, crate::runtime::capabilities::VOLUME)
                                    .expect("new volume capability remains owned");
                            }
                            return Err(error);
                        }
                    }
                }
            }
            if let Err(error) = self.table.replace_roots(capabilities, &metadata, &admitted, true) {
                for handle in created {
                    capabilities
                        .table
                        .remove(handle, crate::runtime::capabilities::VOLUME)
                        .expect("new volume capability remains owned");
                }
                return Err(error);
            }
            for prior in &self.roots {
                if !roots.iter().any(|root| root.handle == prior.handle) {
                    capabilities
                        .table
                        .remove(prior.handle, crate::runtime::capabilities::VOLUME)
                        .expect("retired root owns volume capability");
                }
            }
            for root in roots.iter() {
                capabilities
                    .replace_volume(root.handle, root.metadata.clone())
                    .expect("admitted volume capability remains live");
            }
            Ok(())
        })
    }
    fn bundle_root(&mut self, id: [u8; 16]) -> Result<(Root, bool)> {
        if let Some(prior) = self.roots.iter().find(|r| r.metadata.backing == abi::VOLUME_BUNDLE) {
            let root = Root {
                volume: prior.volume,
                admitted: prior.admitted.clone(),
                metadata: prior.metadata.clone(),
                slot: 255,
                handle: prior.handle,
            };
            return Ok((root, false));
        }
        let package = self.bundle.as_ref().ok_or(Error::Unavailable)?.0.clone();
        let object =
            objects::Object::Bundle(Box::new(crate::package::filesystem::Object::root(package)));
        let engine = self.objects.allocate(1, Rights::READ, Rc::new(object))?.raw();
        let metadata = AvailableVolume {
            backing: abi::VOLUME_BUNDLE,
            source_epoch: [0; 16],
            id,
            name: "Bundle".into(),
            tags: vec![],
            rights: VolumeRights::READ,
            is_root: false,
            role: 0,
        };
        let admitted = runtime::Root {
            replaces: None,
            backing: abi::VOLUME_BUNDLE,
            volume: id,
            grant: new_bundle_id(),
            engine,
            writable: false,
            credit: self.table.capacity().one(runtime::capacity::Kind::File)?,
        };
        Ok((
            Root {
                volume: Id::from_bytes(id),
                admitted,
                metadata,
                slot: 255,
                handle: Handle::default(),
            },
            true,
        ))
    }
    pub(super) fn metadata(&self) -> impl Iterator<Item = (Handle, &AvailableVolume, [u8; 16])> {
        self.roots.iter().map(|root| (root.handle, &root.metadata, root.admitted.grant))
    }
    fn enumerate(&mut self, revision: u64) -> Result<Vec<FileHandle>> {
        if revision != self.revision {
            return Err(Error::Conflict);
        }
        let ids = self.roots.iter().map(|root| root.metadata.id).collect::<Vec<_>>();
        work::capabilities(|capabilities| self.table.enumerate(capabilities, &ids))
    }
}

fn root_metadata(host: &Host, slot: usize, volume: &Volume) -> Result<AvailableVolume> {
    host.admit_volume(volume, Permission::Read)?;
    let (source_epoch, writable) = if slot == 0 {
        ([0; 16], true)
    } else {
        let selections = host.selections.borrow();
        let selected = selections
            .get(&host.session.borrow().id)
            .and_then(|grants| grants.iter().find(|grant| grant.volume == volume.id))
            .ok_or(Error::AccessDenied)?;
        (*selected.incarnation.as_bytes(), selected.write)
    };
    let (name, tag, role) = if slot != 0 {
        ("Personal", "personal", 0)
    } else if host.session.borrow().shared_volume.is_some() {
        ("Shared", "session", abi::VolumeRole::Session as u32)
    } else {
        ("Root", "root", 0)
    };
    Ok(AvailableVolume {
        backing: abi::VOLUME_PLAYBIT,
        source_epoch,
        id: *volume.id.as_bytes(),
        name: name.into(),
        tags: vec![tag.into()],
        rights: VolumeRights(if slot == 0 { 7 } else { 1 | if writable { 2 } else { 0 } }),
        is_root: slot == 0 && role == 0,
        role,
    })
}

/// Guest output storage was admitted in JavaScript before acquiring any aliases.
/// The expected revision binds those capacities to the retained metadata cut.
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_volumes_acquire(revision: u64) -> i32 {
    operations::call(|host, _| {
        let handles = host.files.borrow_mut().enumerate(revision)?;
        let mut output = OUTPUT.lock().expect("mutex is not poisoned");
        output.clear();
        for file in handles {
            output.extend_from_slice(&file.0.to_le_bytes());
        }
        Ok(())
    })
}

/// Directory aliases own no I/O. Removing one does not close its helper or
/// another alias; revoked handles remain disposable until their owner drains.
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_dispose(generation: u32, slot: u32) -> i32 {
    operations::call(|host, _| {
        let mut files = host.files.borrow_mut();
        let resource = work::capabilities(|capabilities| {
            files.table.remove_file(capabilities, FileHandle::new(generation, slot))
        })?;
        if !resource.alias
            && let Some(engine) = resource.engine
        {
            files.objects.take(&engine);
        }
        Ok(())
    })
}

// JSON transports opaque 63-bit identities as decimal strings. JavaScript must
// never round them through Number before returning them to the Rust owner.
fn engine_json<S: serde::Serializer>(
    engine: &u64,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&engine.to_string())
}
fn output_json<S: serde::Serializer>(
    output: &runtime::Output,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    let mut value = serde_json::to_value(output).map_err(serde::ser::Error::custom)?;
    match output {
        runtime::Output::File { handle } => {
            value["File"]["handle"] = serde_json::Value::String(handle.to_string())
        }
        runtime::Output::List { handle } => {
            value["List"]["handle"] = serde_json::Value::String(handle.to_string())
        }
        runtime::Output::Close { recovery: Some(handle), .. } => {
            value["Close"]["recovery"] = serde_json::Value::String(handle.to_string())
        }
        _ => {}
    }
    value.serialize(serializer)
}
struct JsonOutput<'a>(&'a runtime::Output);
impl Serialize for JsonOutput<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        output_json(self.0, serializer)
    }
}
