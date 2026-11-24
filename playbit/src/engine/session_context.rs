//! Session context records shared by native and Wasm app hosts.
use super::Result;
use crate::{
    runtime::syscall as rt,
    store::{LocalStore, PersonalVolumeGrant, Session},
};

/// Current consent can narrow an instance's captured ceiling, never expand it.
pub(super) fn selected_grants(
    store: &LocalStore,
    session: &Session,
    ceiling: &[PersonalVolumeGrant],
) -> Result<Vec<PersonalVolumeGrant>> {
    let selected = store.personal_volume_grants(&session.id)?.unwrap_or_default();
    let access = crate::store::Access::session_version(
        store.account().id,
        &session.id,
        &session.resolved_build_id,
    );
    let mut grants = Vec::with_capacity(ceiling.len());
    for admitted in ceiling {
        let Some(current) = selected.iter().find(|grant| grant.volume == admitted.volume) else {
            continue;
        };
        match store.admit_binding(&access, admitted.volume) {
            Ok(_) => {}
            Err(crate::store::Error::AccessDenied(_)) => continue,
            Err(error) => return Err(error.into()),
        }
        if let Some(grant) = narrow_grant(admitted, current) {
            grants.push(grant);
        }
    }
    Ok(grants)
}
fn narrow_grant(
    admitted: &PersonalVolumeGrant,
    current: &PersonalVolumeGrant,
) -> Option<PersonalVolumeGrant> {
    (admitted.volume == current.volume).then_some(PersonalVolumeGrant {
        volume: admitted.volume,
        write: admitted.write && current.write,
    })
}

fn automatic_volume(
    store: &LocalStore,
    session: &Session,
    slot: &crate::store::AutomaticVolumeSlot,
) -> Result<rt::AvailableVolume> {
    use crate::package::AutomaticVolumeRole as Role;
    let name = match slot.role {
        Role::Root => "World",
        Role::Session => "Shared",
        Role::User => "User",
        Role::UserSession => "User session",
        Role::Local => "Local",
        Role::LocalSession => "Local session",
    };
    let stored = match slot.state {
        crate::store::AutomaticVolumeState::Ready => Some(store.volume(slot.volume)?),
        _ => None,
    };
    let rights = automatic_rights(store, session, slot.role, slot.volume, slot.overridden)?;
    Ok(rt::AvailableVolume {
        backing: playbit_api::abi::data::VOLUME_PLAYBIT,
        source_epoch: [0; 16],
        id: *slot.volume.as_bytes(),
        name: stored.map_or_else(|| name.to_owned(), |volume| volume.name),
        tags: vec![
            match slot.role {
                Role::Root => "root",
                Role::Session => "session",
                Role::User => "user",
                Role::UserSession => "user-session",
                Role::Local => "local",
                Role::LocalSession => "local-session",
            }
            .into(),
        ],
        rights,
        is_root: slot.role == Role::Root,
        role: slot.role as u32,
    })
}

pub(super) fn automatic_rights(
    store: &LocalStore,
    session: &Session,
    role: crate::package::AutomaticVolumeRole,
    volume: crate::store::Id,
    overridden: bool,
) -> Result<rt::VolumeRights> {
    use crate::package::AutomaticVolumeRole as Role;
    if overridden {
        use crate::store::object_contract::{Permission, Principal};
        let access = crate::store::Access::session_version(
            store.account().id,
            &session.id,
            &session.resolved_build_id,
        );
        let details = store.volume_details(&access, volume)?;
        return Ok(if details.volume.owner == store.account().id {
            rt::VolumeRights::ALL
        } else if details
            .permissions
            .permits(Some(Principal::Human(store.account().id)), Permission::Write)
        {
            rt::VolumeRights(
                rt::VolumeRights::READ.0 | rt::VolumeRights::WRITE.0 | rt::VolumeRights::SCHEMA.0,
            )
        } else {
            rt::VolumeRights::READ
        });
    }
    let writable = role != Role::Root
        || match store.volume(volume) {
            Ok(volume) => volume.owner == store.account().id,
            Err(crate::store::Error::NotFound(_)) => session.app.publisher == store.account().id,
            Err(error) => return Err(error.into()),
        };
    // Only the canonical publisher World defers KV mutation to each entry's ACL.
    // An explicit Root returned above retains the caller's chosen ceiling.
    Ok(if writable {
        rt::VolumeRights::ALL
    } else {
        rt::VolumeRights(rt::VolumeRights::READ.0 | rt::VolumeRights::KV_WRITE.0)
    })
}

pub(super) const fn personal_rights(write: bool) -> rt::VolumeRights {
    if write {
        rt::VolumeRights(rt::VolumeRights::READ.0 | rt::VolumeRights::WRITE.0)
    } else {
        rt::VolumeRights::READ
    }
}

pub(super) fn volumes(
    store: &LocalStore,
    session: &Session,
    grants: &[PersonalVolumeGrant],
) -> Result<Vec<rt::AvailableVolume>> {
    let local = store.has_automatic_scopes(&session.id)? || session.shared_volume.is_some();
    let slots = if local { store.automatic_volumes(&session.id)? } else { Vec::new() };
    let scope_count = if local { slots.len() } else { 1 };
    if grants.len() > playbit_api::abi::syscall::MAX_VOLUMES - scope_count - 1 {
        return Err("Too many personal volumes for this app".into());
    }
    let mut volumes = Vec::with_capacity(scope_count + grants.len() + 1);
    if local {
        for slot in &slots {
            volumes.push(automatic_volume(store, session, slot)?);
        }
    } else {
        let id = session.root_volume;
        let volume = store.volume(id)?;
        volumes.push(rt::AvailableVolume {
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            source_epoch: [0; 16],
            id: *id.as_bytes(),
            name: volume.name,
            tags: vec!["root".into()],
            rights: rt::VolumeRights::ALL,
            is_root: true,
            role: 0,
        });
    }
    let selections = store.personal_volume_selections(&session.id)?.unwrap_or_default();
    for grant in grants {
        if slots.iter().any(|slot| slot.volume == grant.volume) {
            return Err("Additional volumes must be distinct from automatic scopes".into());
        }
        let Some(selection) = selections.iter().find(|selection| selection.volume == grant.volume)
        else {
            continue;
        };
        let volume = store.volume(grant.volume)?;
        volumes.push(rt::AvailableVolume {
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            source_epoch: *selection.incarnation.as_bytes(),
            id: *volume.id.as_bytes(),
            name: volume.name,
            tags: vec!["personal".into()],
            rights: personal_rights(grant.write && selection.write),
            is_root: false,
            role: 0,
        });
    }
    rt::validate_volumes(&volumes).map_err(|error| format!("Invalid volume context: {error:?}"))?;
    Ok(volumes)
}

#[cfg(any(feature = "app-host", feature = "connected-engine"))]
pub(super) fn decode_session_id(value: &str) -> Result<[u8; 32]> {
    crate::identifier::decode(value).map_err(Into::into)
}

pub(super) fn bundle_volume(id: [u8; 16]) -> rt::AvailableVolume {
    rt::AvailableVolume {
        backing: playbit_api::abi::data::VOLUME_BUNDLE,
        source_epoch: [0; 16],
        id,
        name: "Bundle".into(),
        tags: vec![],
        rights: rt::VolumeRights::READ,
        is_root: false,
        role: 0,
    }
}
