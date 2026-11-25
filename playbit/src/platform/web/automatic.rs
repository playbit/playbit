//! The guest sees stable role addresses before opening any OPFS backing.
//! Only an accepted data or File operation requests first-use work.
use super::*;
use crate::package::AutomaticVolumeRole as Role;
use crate::runtime::host::{AvailableVolume, VolumeRights};
use crate::store::initialization::RootInitialization;
use playbit_api::abi::data::VOLUME_PLAYBIT;

const ROLES: [Role; 6] =
    [Role::Root, Role::Session, Role::User, Role::UserSession, Role::Local, Role::LocalSession];

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Address {
    role: Role,
    volume: Id,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Lazy,
    Requested,
    Preparing,
    Ready,
    Failed(Error),
}

struct Scope {
    address: Address,
    state: State,
    initializer: Option<Rc<RootInitialization>>,
    override_rights: Option<VolumeRights>,
}

#[derive(Default)]
pub(super) struct Scopes {
    slots: Vec<Scope>,
    pub(super) enabled: bool,
}

impl Host {
    pub(super) fn has_automatic(&self) -> bool {
        self.automatic.borrow().enabled
    }

    pub(super) fn bind_automatic(&self) -> Result<()> {
        let session = self.session.borrow();
        let id = crate::identifier::decode(&session.id).map_err(map_error)?;
        let mut automatic = self.automatic.borrow_mut();
        if !automatic.slots.is_empty() {
            automatic.enabled = true;
            // Session adoption changes the admitted build, never its identity.
            return Ok(());
        }
        let primary = session.replica_volume();
        let mut slots = Vec::with_capacity(ROLES.len());
        for role in ROLES {
            let derived = session
                .app
                .volume_id(
                    role,
                    role.is_session().then_some(id),
                    role.has_account().then_some(self.account),
                    role.is_local().then_some(self.installation),
                )
                .map_err(map_error)?;
            let volume = match role {
                Role::Root => session.root_volume,
                Role::User => self.user_volume.unwrap_or(derived),
                _ => derived,
            };
            if slots.iter().any(|scope: &Scope| scope.address.volume == volume) {
                return Err(Error::Conflict);
            }
            let state = if role == Role::User && self.deleted_user_volume(volume) {
                State::Failed(Error::AccessDenied)
            } else if volume == primary {
                State::Ready
            } else if role == Role::Session
                && session.shared_volume.is_none()
                && session.creator != self.account
            {
                State::Failed(Error::AccessDenied)
            } else {
                State::Lazy
            };
            let overridden = (role == Role::Root && derived != volume)
                || (role == Role::User && self.user_volume.is_some());
            let override_rights = if overridden {
                let stored = self.volume_objects.borrow();
                Some(match stored.get(&volume) {
                    Some(value) if value.record.owner == self.account => VolumeRights::ALL,
                    Some(value) if volume == primary => self.override_rights(value)?,
                    _ => VolumeRights::READ,
                })
            } else {
                None
            };
            slots.push(Scope {
                address: Address { role, volume },
                state,
                initializer: None,
                override_rights,
            });
        }
        automatic.enabled = true;
        automatic.slots = slots;
        Ok(())
    }

    fn override_rights(&self, volume: &Volume) -> Result<VolumeRights> {
        let policy = MetadataPolicy::read(&volume.index).map_err(store_error)?;
        if !policy.volume().same_identity(&volume.record) {
            return Err(Error::AccessDenied);
        }
        if policy.volume().owner == self.account {
            return Ok(VolumeRights::ALL);
        }
        match policy.authorize(self.account, Permission::Write) {
            Ok(()) => Ok(VolumeRights(
                VolumeRights::READ.0 | VolumeRights::WRITE.0 | VolumeRights::SCHEMA.0,
            )),
            Err(crate::store::Error::AccessDenied(_)) => Ok(VolumeRights::READ),
            Err(error) => Err(store_error(error)),
        }
    }

    pub(super) fn update_override_rights(&self, volume: &Volume) -> Result<()> {
        let mut scopes = self.automatic.borrow_mut();
        if let Some(scope) = scopes
            .slots
            .iter_mut()
            .find(|scope| scope.address.volume == volume.id && scope.override_rights.is_some())
        {
            scope.override_rights = Some(self.override_rights(volume)?);
        }
        Ok(())
    }

    pub(super) fn automatic_metadata(&self) -> Vec<AvailableVolume> {
        let automatic = self.automatic.borrow();
        if !automatic.enabled {
            return Vec::new();
        }
        let session = self.session.borrow();
        automatic
            .slots
            .iter()
            .map(|scope| {
                let role = scope.address.role;
                let (tag, name) = match role {
                    Role::Root => ("root", "World"),
                    Role::Session => ("session", "Shared"),
                    Role::User => ("user", "User"),
                    Role::UserSession => ("user-session", "User session"),
                    Role::Local => ("local", "Local"),
                    Role::LocalSession => ("local-session", "Local session"),
                };
                let volume = scope.address.volume;
                let rights = if let Some(rights) = scope.override_rights {
                    rights
                } else if role == Role::Root
                    && (session.app.publisher != self.account
                        || session.is_distinct_world(volume)
                            && self
                                .volume_objects
                                .borrow()
                                .get(&volume)
                                .is_none_or(|v| self.is_projected_world(v)))
                {
                    VolumeRights(VolumeRights::READ.0 | VolumeRights::KV_WRITE.0)
                } else {
                    VolumeRights::ALL
                };
                AvailableVolume {
                    backing: VOLUME_PLAYBIT,
                    source_epoch: [0; 16],
                    id: *volume.as_bytes(),
                    name: name.into(),
                    tags: vec![tag.into()],
                    rights,
                    is_root: role == Role::Root,
                    role: role as u32,
                }
            })
            .collect()
    }

    pub(super) fn is_projected_world(&self, volume: &Volume) -> bool {
        self.session.borrow().is_distinct_world(volume.id)
            && volume.key == DatabaseId::world_projection_key(volume.id)
    }

    pub(super) fn automatic_role(&self, volume: Id) -> Option<Role> {
        self.automatic
            .borrow()
            .slots
            .iter()
            .find(|s| s.address.volume == volume)
            .map(|s| s.address.role)
    }

    pub(super) fn guest_volume(&self, volume: Id) -> Result<Rc<Volume>> {
        let mut scopes = self.automatic.borrow_mut();
        if scopes.enabled
            && let Some(scope) = scopes.slots.iter_mut().find(|s| s.address.volume == volume)
        {
            match scope.state {
                State::Ready => {}
                State::Lazy => {
                    scope.state = State::Requested;
                    return Err(Error::WouldBlock);
                }
                State::Requested | State::Preparing => return Err(Error::WouldBlock),
                State::Failed(error) => {
                    if error == Error::Busy {
                        scope.state = State::Lazy;
                    }
                    return Err(error);
                }
            }
        }
        self.volume_objects.borrow().get(&volume).cloned().ok_or(Error::Unsupported)
    }

    pub(super) fn retry_automatic(&self) {
        let session = self.session.borrow();
        for scope in &mut self.automatic.borrow_mut().slots {
            if let State::Failed(error) = scope.state
                && error != Error::AccessDenied
            {
                scope.state = if error == Error::Unavailable
                    && session.is_distinct_world(scope.address.volume)
                {
                    State::Requested
                } else {
                    State::Lazy
                };
            }
        }
    }

    pub(super) fn initializer_for(&self, volume: Id) -> Option<Rc<RootInitialization>> {
        if self.volumes.borrow()[0].id == volume {
            return self.initializer.clone();
        }
        self.automatic
            .borrow()
            .slots
            .iter()
            .find(|s| s.address.volume == volume)
            .and_then(|s| s.initializer.clone())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Register {
    role: Role,
    volume: VolumeRecord,
    base: u32,
    replica_ids: [u64; 2],
    key: Option<[u8; 32]>,
    creation_build: Option<String>,
    creation_slot: Option<u32>,
}

impl Register {
    fn world_key(&self, session: &Session, account: Id) -> Result<[u8; 32]> {
        session.validate_publisher_world(self.volume.id).map_err(store_error)?;
        if self.volume.owner != session.app.publisher
            || !self.volume.share_key_with_service
            || self.volume.personal
        {
            return Err(Error::AccessDenied);
        }
        Ok(match self.key {
            Some(key) if self.volume.owner == account => key,
            None => DatabaseId::world_projection_key(self.volume.id),
            _ => return Err(Error::AccessDenied),
        })
    }
}

impl Host {
    fn register_automatic(&self, request: Register) -> Result<Vec<u8>> {
        self.authorize(&self.volumes.borrow()[0], None, Permission::Read)?;
        let id = request.volume.id;
        let mut scopes = self.automatic.borrow_mut();
        let scope = scopes
            .slots
            .iter_mut()
            .find(|s| s.address.volume == id && s.address.role == request.role)
            .ok_or(Error::AccessDenied)?;
        let current = self.session.borrow();
        // The worker renews a previously requested World after a fresh AP Open.
        let reopening =
            matches!(scope.state, State::Failed(Error::AccessDenied | Error::Unavailable))
                && request.role == Role::Root
                && current.is_distinct_world(id);
        if !reopening && !matches!(scope.state, State::Requested | State::Preparing) {
            return Err(Error::Conflict);
        }
        let override_user = request.role == Role::User && self.user_volume.is_some();
        let world = request.role == Role::Root;
        if (world || override_user)
            && (request.creation_build.is_some() || request.creation_slot.is_some())
        {
            return Err(Error::AccessDenied);
        }
        let descriptor = &request.volume;
        let key = if world {
            request.world_key(&current, self.account)?
        } else {
            if (!override_user
                && (descriptor.owner != self.account
                    || descriptor.personal
                    || descriptor.share_key_with_service))
                || (request.role == Role::Session && current.creator != self.account)
            {
                return Err(Error::AccessDenied);
            }
            request.key.ok_or(Error::InvalidArgument)?
        };
        let volume = self.open_secondary(request.volume, key, request.base, request.replica_ids)?;
        if !world && !override_user {
            // The creator can also attest to a session's first build. Invitees
            // resume an interrupted initializer from the volume's own descriptor.
            let descriptor_build = request.creation_build.as_deref();
            let first_build = if request.role.is_session() {
                self.creation_build.as_deref().or(descriptor_build)
            } else {
                descriptor_build
            };
            scope.initializer = initialize(
                self,
                &volume,
                request.role,
                first_build,
                request.creation_build.as_deref(),
                request.creation_slot,
            )?;
        }
        let output = serde_json::to_vec(&scope.initializer.as_deref()).map_err(map_error)?;
        self.secondary_bases.borrow_mut().insert(id, request.base);
        self.volume_objects.borrow_mut().insert(id, volume);
        scope.state = State::Preparing;
        Ok(output)
    }
}

// Local metadata is never replicated. Its first byte distinguishes interrupted
// publication from a committed initializer; the next 32 are the exact build.
const INITIALIZATION: &[u8] = b"web-automatic-initialization-v1";
fn initialize(
    host: &Host,
    volume: &Volume,
    role: Role,
    first_build: Option<&str>,
    creation_build: Option<&str>,
    slot: Option<u32>,
) -> Result<Option<Rc<RootInitialization>>> {
    let read = volume.index.begin_read().map_err(map_error)?;
    let marker = read.local_get(INITIALIZATION).map_err(map_error)?;
    let exists = read.has(&crate::store::catalog::VOLUME_KEY).map_err(map_error)?;
    let build = match marker.as_deref() {
        Some([1, build @ ..]) if exists && build.len() == 32 => return Ok(None),
        Some([0, build @ ..]) if exists && build.len() == 32 => crate::identifier::encode(build),
        None if !exists => first_build.ok_or(Error::Conflict)?.to_owned(),
        _ => return Err(Error::Conflict),
    };
    if Some(build.as_str()) != creation_build {
        return Err(Error::Conflict);
    }
    let initializer = package::scope_initializer(host, slot, &build, role)?;
    drop(read);
    if !exists {
        let policy =
            MetadataPolicy::new(volume.record.clone(), initializer.volume_permissions(false))
                .map_err(store_error)?;
        let mut tx = volume.index.begin_write(unsafe { now() } as u64).map_err(map_error)?;
        policy
            .write_entries(&mut tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)
            .map_err(store_error)?;
        let mut marker = [0u8; 33];
        marker[1..].copy_from_slice(&crate::identifier::decode::<32>(&build).map_err(map_error)?);
        tx.local_indexes().put(INITIALIZATION, &marker).map_err(map_error)?;
        tx.commit().map_err(map_error)?;
    }
    Ok(Some(Rc::new(initializer)))
}

pub(super) fn with_host(
    length: usize,
    operation: impl FnOnce(&Host, &[u8]) -> Result<Vec<u8>>,
) -> i32 {
    crate::platform::failure::guard();
    let result = (|| {
        let input = admit_input(length, 1024)?;
        let host = HOST.with(|cell| cell.borrow().clone()).ok_or(Error::Unavailable)?;
        operation(&host, &input[..length])
    })();
    result.map_or_else(|e| e as i32, publish_output)
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_automatic_needed() -> i32 {
    with_host(0, |host, _| {
        let scopes = host.automatic.borrow();
        let needed: Vec<_> = scopes
            .slots
            .iter()
            .filter(|s| s.state == State::Requested)
            .map(|s| s.address)
            .collect();
        serde_json::to_vec(&needed).map_err(map_error)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_automatic_register(length: usize) -> i32 {
    with_host(length, |host, bytes| {
        host.register_automatic(serde_json::from_slice(bytes).map_err(map_error)?)
    })
}

/// A fresh, authenticated World Open withdrew the session-derived attachment.
/// Preserve the published OPFS files for a later admitted capture.
#[unsafe(no_mangle)]
pub extern "C" fn pb_automatic_world_denied(length: usize) -> i32 {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Denial {
        volume: Id,
        revoked: bool,
    }
    with_host(length, |host, bytes| {
        let Denial { volume, revoked } = serde_json::from_slice(bytes).map_err(map_error)?;
        let session = host.session.borrow();
        if !session.is_distinct_world(volume) {
            return Err(Error::AccessDenied);
        }
        session.validate_publisher_world(volume).map_err(store_error)?;
        let mut scopes = host.automatic.borrow_mut();
        let slot = scopes
            .slots
            .iter_mut()
            .find(|s| s.address.role == Role::Root && s.address.volume == volume)
            .ok_or(Error::AccessDenied)?;
        if slot.state == State::Failed(Error::AccessDenied) {
            return Ok(Vec::new());
        }
        if !matches!(slot.state, State::Preparing | State::Ready) {
            return Err(Error::Conflict);
        }
        slot.state = State::Failed(Error::AccessDenied);
        let fence = if revoked {
            // AP revoked participation in this World only. Shared has its own authority.
            authority::terminal(
                host,
                volume,
                None,
                volume,
                crate::store::authority::TerminalCause::ParticipantDenied as u8,
            )
        } else {
            Ok(())
        };
        host.changes.borrow_mut().discard_volume(volume);
        host.discard_named_backings(volume);
        host.volume_objects.borrow_mut().remove(&volume);
        host.secondary_bases.borrow_mut().remove(&volume);
        storage::invalidate();
        fence?;
        Ok(Vec::new())
    })
}

/// An authenticated account tombstone permanently withdraws its explicit User
/// binding for this host, including guest handles and pending publications.
#[unsafe(no_mangle)]
pub extern "C" fn pb_automatic_user_deleted(length: usize) -> i32 {
    with_host(length, |host, bytes| {
        let volume: Id = serde_json::from_slice(bytes).map_err(map_error)?;
        if host.user_volume != Some(volume) || volume == host.session.borrow().replica_volume() {
            return Err(Error::AccessDenied);
        }
        host.user_volume_deleted.set(true);
        if let Some(slot) = host
            .automatic
            .borrow_mut()
            .slots
            .iter_mut()
            .find(|s| s.address.role == Role::User && s.address.volume == volume)
        {
            slot.state = State::Failed(Error::AccessDenied);
        }
        host.changes.borrow_mut().discard_volume(volume);
        host.personal.borrow_mut().retain(|registered| registered.id != volume);
        host.discard_named_backings(volume);
        host.volume_objects.borrow_mut().remove(&volume);
        host.secondary_bases.borrow_mut().remove(&volume);
        storage::invalidate();
        Ok(Vec::new())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_automatic_ready(length: usize) -> i32 {
    with_host(length, |host, bytes| {
        let address: Address = serde_json::from_slice(bytes).map_err(map_error)?;
        let volume = host.replica_volume(address.volume)?;
        host.authorize(&volume, None, Permission::Read)?;
        host.update_override_rights(&volume)?;
        let mut scopes = host.automatic.borrow_mut();
        let slot = scopes
            .slots
            .iter_mut()
            .find(|s| s.address.role == address.role && s.address.volume == address.volume)
            .ok_or(Error::AccessDenied)?;
        if slot.state != State::Preparing {
            return Err(Error::Conflict);
        }
        if let Some(declaration) = &slot.initializer {
            let snap = crate::store::volume_index::IndexSnapshot::new(
                volume.index.begin_read().map_err(map_error)?,
            );
            for entry in declaration
                .databases
                .iter()
                .map(|e| e.id)
                .chain(declaration.blobs.iter().map(|e| e.id))
            {
                if !snap.backing_ready(entry).map_err(store_error)? {
                    return Err(Error::Unavailable);
                }
            }
        }
        if slot.initializer.is_some() {
            let mut tx = volume.index.begin_write(unsafe { now() } as u64).map_err(map_error)?;
            let mut local = tx.local_indexes();
            let Some(marker) = local.get(INITIALIZATION).map_err(map_error)? else {
                return Err(Error::Conflict);
            };
            let mut marker: [u8; 33] = marker.as_ref().try_into().map_err(|_| Error::Conflict)?;
            if marker[0] != 0 {
                return Err(Error::Conflict);
            }
            marker[0] = 1;
            local.put(INITIALIZATION, &marker).map_err(map_error)?;
            tx.commit().map_err(map_error)?;
        }
        slot.initializer = None;
        slot.state = State::Ready;
        Ok(Vec::new())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_automatic_failed(length: usize) -> i32 {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Failure {
        role: Role,
        volume: Id,
        status: i32,
    }
    with_host(length, |host, bytes| {
        let error: Failure = serde_json::from_slice(bytes).map_err(map_error)?;
        // A preparation that yielded before account deletion may finish afterward.
        if error.role == Role::User && host.deleted_user_volume(error.volume) {
            return Ok(Vec::new());
        }
        let status = Error::try_from(error.status).map_err(|_| Error::InvalidArgument)?;
        if matches!(status, Error::WouldBlock | Error::BufferTooSmall) {
            return Err(Error::InvalidArgument);
        }
        let mut scopes = host.automatic.borrow_mut();
        let slot = scopes
            .slots
            .iter_mut()
            .find(|s| s.address.role == error.role && s.address.volume == error.volume)
            .ok_or(Error::AccessDenied)?;
        if !matches!(slot.state, State::Requested | State::Preparing) {
            return Err(Error::Conflict);
        }
        slot.state = State::Failed(status);
        slot.initializer = None;
        if !host.personal.borrow().iter().any(|volume| volume.id == error.volume) {
            host.discard_named_backings(error.volume);
            host.volume_objects.borrow_mut().remove(&error.volume);
            host.secondary_bases.borrow_mut().remove(&error.volume);
            storage::invalidate();
        }
        Ok(Vec::new())
    })
}
