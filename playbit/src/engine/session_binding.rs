//! Captured app build authority, revalidated before each app turn.
#[cfg(feature = "connected-engine")]
use super::Services;
use crate::runtime::{host::*, syscall::HostError};
use crate::store::{Access, Id as StoreId};
use crate::store::{LocalStore, PersonalVolumeGrant, Session};
#[cfg(feature = "connected-engine")]
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) struct Binding {
    store: Arc<LocalStore>,
    session: Session,
    pub(crate) access: Access,
    grants: Vec<PersonalVolumeGrant>,
    incarnations: Vec<(StoreId, StoreId)>,
    ceiling: Vec<PersonalVolumeGrant>,
    names: u64,
    bundle_id: [u8; 16],
    automatic: [StoreId; 6],
}
impl Binding {
    #[cfg(feature = "connected-engine")]
    pub(crate) fn delivery_volumes(&self) -> [StoreId; 6] {
        self.automatic
    }
    #[cfg(feature = "connected-engine")]
    pub(crate) fn prepare(
        services: &Services,
        id: &str,
        package: &crate::package::Package,
    ) -> HostResult<(Self, GuestContext)> {
        let session = services.store.session(id).map_err(crate::runtime::store_bridge::error)?;
        if package.id().map_err(|_| HostError::InvalidArgument)? != session.resolved_build_id
            || package.app_id() != session.app.app_id
        {
            return Err(HostError::Conflict);
        }
        let access =
            Access::session_version(services.store.account().id, id, &session.resolved_build_id);
        services
            .store
            .admit_binding(&access, session.root_volume)
            .map_err(crate::runtime::store_bridge::error)?;
        let grants = grants(&services.store, &session, &access)?;
        check_metadata(
            &session,
            grants.capacity().checked_mul(2).ok_or(HostError::LimitExceeded)?,
        )?;
        let mut volumes = super::session_context::volumes(&services.store, &session, &grants)
            .map_err(context_error)?;
        let automatic = if session.shared_volume.is_some()
            || services
                .store
                .has_automatic_scopes(&session.id)
                .map_err(crate::runtime::store_bridge::error)?
        {
            let scopes = volumes.get(..6).ok_or(HostError::InvalidArgument)?;
            std::array::from_fn(|index| StoreId::from_bytes(scopes[index].id))
        } else {
            [session.root_volume; 6]
        };
        let bundle_id =
            *StoreId::generate().map_err(crate::runtime::store_bridge::error)?.as_bytes();
        volumes.push(super::session_context::bundle_volume(bundle_id));
        let incarnations = incarnations(&services.store, &session, &grants)?;
        let context = GuestContext {
            app_name: package.name().to_owned(),
            app_build: Some(
                crate::identifier::decode(&session.resolved_build_id)
                    .map_err(|_| HostError::InvalidArgument)?,
            ),
            identity: Identity {
                account_id: *services.store.account().id.as_bytes(),
                is_service: false,
                device_id: *services.store.account().id.as_bytes(),
                instance_id: *StoreId::generate()
                    .map_err(crate::runtime::store_bridge::error)?
                    .as_bytes(),
                session_id: Some(
                    super::session_context::decode_session_id(&session.id)
                        .map_err(context_error)?,
                ),
            },
            volumes,
            startup_arguments: BTreeMap::new(),
            trace_catalog: package.trace().cloned(),
            required_capabilities: package.required_host_features().iter().cloned().collect(),
            participants: vec![],
        };
        Ok((
            Self {
                store: services.store.clone(),
                session,
                access,
                ceiling: grants.clone(),
                grants,
                incarnations,
                names: services.store.volume_names_generation(),
                bundle_id,
                automatic,
            },
            context,
        ))
    }
    /// Return a new context only when consent or volume labels changed. The
    /// root/build admission remains current even when no notification was seen.
    pub(crate) fn refresh(&mut self) -> HostResult<Option<Vec<AvailableVolume>>> {
        let current =
            self.store.session(&self.session.id).map_err(crate::runtime::store_bridge::error)?;
        if current.resolved_build_id != self.session.resolved_build_id
            || current.root_volume != self.session.root_volume
            || current.shared_volume != self.session.shared_volume
        {
            return Err(HostError::Conflict);
        }
        self.store
            .admit_binding(&self.access, self.session.root_volume)
            .map_err(crate::runtime::store_bridge::error)?;
        let grants =
            super::session_context::selected_grants(&self.store, &self.session, &self.ceiling)
                .map_err(context_error)?;
        check_metadata(
            &self.session,
            grants
                .capacity()
                .checked_add(self.ceiling.capacity())
                .ok_or(HostError::LimitExceeded)?,
        )?;
        let incarnations = incarnations(&self.store, &self.session, &grants)?;
        let names = self.store.volume_names_generation();
        if grants == self.grants && incarnations == self.incarnations && names == self.names {
            return Ok(None);
        }
        let mut volumes = super::session_context::volumes(&self.store, &self.session, &grants)
            .map_err(context_error)?;
        volumes.push(super::session_context::bundle_volume(self.bundle_id));
        self.grants = grants;
        self.incarnations = incarnations;
        self.names = names;
        Ok(Some(volumes))
    }
}
fn incarnations(
    store: &LocalStore,
    session: &Session,
    grants: &[PersonalVolumeGrant],
) -> HostResult<Vec<(StoreId, StoreId)>> {
    let mut values = Vec::with_capacity(grants.len());
    for selected in store
        .personal_volume_selections(&session.id)
        .map_err(crate::runtime::store_bridge::error)?
        .unwrap_or_default()
    {
        if grants.iter().any(|grant| grant.volume == selected.volume) {
            values.push((selected.volume, selected.incarnation));
        }
    }
    if values.capacity() > playbit_api::abi::syscall::MAX_VOLUMES {
        return Err(HostError::LimitExceeded);
    }
    values.sort_unstable();
    Ok(values)
}
#[cfg(feature = "connected-engine")]
fn grants(
    store: &LocalStore,
    session: &Session,
    access: &Access,
) -> HostResult<Vec<PersonalVolumeGrant>> {
    let grants = store
        .personal_volume_grants(&session.id)
        .map_err(crate::runtime::store_bridge::error)?
        .unwrap_or_default();
    if grants.len() > playbit_api::abi::syscall::MAX_VOLUMES - 1 {
        return Err(HostError::LimitExceeded);
    }
    for grant in &grants {
        store.admit_binding(access, grant.volume).map_err(crate::runtime::store_bridge::error)?;
    }
    Ok(grants)
}
fn context_error(error: Box<dyn std::error::Error + Send + Sync>) -> HostError {
    match error.downcast::<crate::store::Error>() {
        Ok(error) => crate::runtime::store_bridge::error(*error),
        Err(_) => HostError::InvalidArgument,
    }
}

// Session/consent bookkeeping, including two bounded scope caches. Context
// strings, resources and runtime state retain their separate runtime budgets.
const METADATA_BYTES: usize = 64 * 1024;
const FIXED_BYTES: usize = std::mem::size_of::<Binding>()
    + playbit_api::abi::syscall::MAX_VOLUMES * std::mem::size_of::<(StoreId, StoreId)>()
    + 2 * playbit_api::abi::syscall::MAX_VOLUMES * std::mem::size_of::<StoreId>()
    + 128; // Access owns one 64-byte session id and one 64-byte build id.
fn metadata_fits(strings: usize, grant_capacity: usize) -> bool {
    grant_capacity
        .checked_mul(std::mem::size_of::<PersonalVolumeGrant>())
        .and_then(|bytes| bytes.checked_add(strings))
        .and_then(|bytes| bytes.checked_add(FIXED_BYTES))
        .is_some_and(|bytes| bytes <= METADATA_BYTES)
}
fn check_metadata(session: &Session, grant_capacity: usize) -> HostResult<()> {
    let selector = match &session.selector {
        crate::store::BuildSelector::Exact { build_id } => build_id.capacity(),
        crate::store::BuildSelector::App { app } => app.app_id.capacity(),
    };
    let strings = session
        .id
        .capacity()
        .checked_add(session.app.app_id.capacity())
        .and_then(|bytes| bytes.checked_add(session.resolved_build_id.capacity()))
        .and_then(|bytes| bytes.checked_add(selector));
    if strings.is_some_and(|strings| metadata_fits(strings, grant_capacity)) {
        Ok(())
    } else {
        Err(HostError::LimitExceeded)
    }
}
