#![allow(
    clippy::unwrap_used,
    reason = "Tests assert current component-rename authority and identities."
)]
use super::*;
use crate::store::object_contract::{Grant, Subject};

fn fixture() -> (tempfile::TempDir, LocalStore, Access, Id) {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let access = Access::owner(store.account.id);
    let volume = store.create_volume("Rename", false).unwrap().id;
    (directory, store, access, volume)
}

#[test]
fn component_rename_retains_nested_parents_children_and_blob_versions_after_restart() {
    // Rename an automatic directory and a leaf in place, preserving every identity and immutable byte version.
    let (directory, store, access, volume) = fixture();
    let file =
        store.create_blob(&access, volume, "foo/bar/Sample Copy.bin", b"sample bytes").unwrap();
    let nested =
        store.create_blob(&access, volume, "foo/bar/nested/child.bin", b"nested bytes").unwrap();
    let foo = store.resolve_entry(&access, volume, "foo").unwrap();
    let bar = store.resolve_entry(&access, volume, "foo/bar").unwrap();
    let address = volume_index::EntryAddress { volume, entry: file.id };
    let version = store.blob_version(&access, address, None).unwrap();
    store.rename_entry_component(&access, volume, bar.id, "bar2").unwrap();
    assert_eq!(store.resolve_entry(&access, volume, "foo").unwrap().id, foo.id);
    assert_eq!(store.resolve_entry(&access, volume, "foo/bar2").unwrap().id, bar.id);
    assert!(store.resolve_entry(&access, volume, "bar2").is_err());
    assert!(store.resolve_entry(&access, volume, "foo/bar").is_err());
    assert_eq!(
        store.resolve_entry(&access, volume, "foo/bar2/Sample Copy.bin").unwrap().id,
        file.id
    );
    assert_eq!(
        store.resolve_entry(&access, volume, "foo/bar2/nested/child.bin").unwrap().id,
        nested.id
    );
    store.rename_entry_component(&access, volume, file.id, "Renamed.bin").unwrap();
    assert_eq!(store.resolve_entry(&access, volume, "foo/bar2/Renamed.bin").unwrap().id, file.id);
    assert!(store.resolve_entry(&access, volume, "Renamed.bin").is_err());
    assert_eq!(store.blob_version(&access, address, None).unwrap(), version);
    assert_eq!(store.read_blob_entry(&access, volume, file.id).unwrap(), b"sample bytes");
    assert_eq!(store.read_blob_entry(&access, volume, nested.id).unwrap(), b"nested bytes");
    assert!(matches!(
        store.rename_entry_component(&access, volume, file.id, "other/path.bin"),
        Err(Error::InvalidInput(_))
    ));
    store.create_blob(&access, volume, "foo/bar2/occupied.bin", b"occupied").unwrap();
    assert!(matches!(
        store.rename_entry_component(&access, volume, file.id, "occupied.bin"),
        Err(Error::AlreadyExists(_))
    ));
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert_eq!(store.resolve_entry(&access, volume, "foo/bar2/Renamed.bin").unwrap().id, file.id);
    assert_eq!(store.blob_version(&access, address, None).unwrap(), version);
    assert_eq!(store.read_blob_entry(&access, volume, file.id).unwrap(), b"sample bytes");
    // The established path operation still performs explicit moves, including a root component.
    store.rename_entry(&access, volume, file.id, "moved/path.bin").unwrap();
    assert_eq!(store.resolve_entry(&access, volume, "moved/path.bin").unwrap().id, file.id);
    store.rename_entry(&access, volume, file.id, "root.bin").unwrap();
    assert_eq!(store.resolve_entry(&access, volume, "root.bin").unwrap().id, file.id);
}

#[test]
fn component_rename_checks_current_original_entry_write_revocation_and_retirement() {
    // Catalog visibility never substitutes for the original entry ACL, revocation fence or live identity.
    let (_directory, store, owner, volume) = fixture();
    let participant = Access::owner(Id::generate().unwrap());
    store
        .set_permissions(
            &owner,
            volume,
            Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read, Permission::Write],
                }],
            },
        )
        .unwrap();
    let file = store.create_blob(&owner, volume, "foo/file.bin", b"contents").unwrap();
    store
        .set_entry_permissions(&owner, volume, file.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    for access in [&owner, &participant] {
        assert!(matches!(
            store.rename_entry_component(access, volume, file.id, "denied.bin"),
            Err(Error::AccessDenied(_))
        ));
    }
    store.set_entry_permissions(&owner, volume, file.id, EntryAcl::Inherit).unwrap();
    store.rename_entry_component(&participant, volume, file.id, "allowed.bin").unwrap();
    assert_eq!(store.resolve_entry(&owner, volume, "foo/allowed.bin").unwrap().id, file.id);
    store.revoke(&owner, volume, participant.account, "withdraw rename access").unwrap();
    assert!(matches!(
        store.rename_entry_component(&participant, volume, file.id, "revoked.bin"),
        Err(Error::ParticipantRevoked { .. })
    ));
    assert_eq!(store.resolve_entry(&owner, volume, "foo/allowed.bin").unwrap().id, file.id);
    store.remove_entry(&owner, volume, file.id).unwrap();
    assert!(matches!(
        store.rename_entry_component(&owner, volume, file.id, "resurrected.bin"),
        Err(Error::NotFound(_))
    ));
    assert!(store.entry_details(&owner, volume, file.id).unwrap().entry.deleted);
}

#[test]
fn component_rename_requires_catalog_read_and_current_personal_write_consent() {
    // Keep the existing Write-only root assignment distinct, and recheck a session's personal-volume grant.
    let (_directory, store, owner, volume) = fixture();
    let writer = Access::owner(Id::generate().unwrap());
    let file = store.create_blob(&owner, volume, "foo/file.bin", b"contents").unwrap();
    store
        .set_entry_permissions(
            &owner,
            volume,
            file.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Principal(Principal::Human(writer.account)),
                    permissions: vec![Permission::Write],
                }],
            }),
        )
        .unwrap();
    assert!(matches!(
        store.rename_entry_component(&writer, volume, file.id, "denied.bin"),
        Err(Error::AccessDenied(_))
    ));
    store.rename_entry(&writer, volume, file.id, "root-only.bin").unwrap();
    store.set_entry_permissions(&owner, volume, file.id, EntryAcl::Inherit).unwrap();
    assert_eq!(store.resolve_entry(&owner, volume, "root-only.bin").unwrap().id, file.id);
    let root = store.create_volume("App root", false).unwrap();
    let version = crate::identifier::encode(&[0x11; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: owner.account, app_id: "test".into() },
            BuildSelector::Exact { build_id: version.clone() },
            &version,
            root.id,
        )
        .unwrap();
    let access = Access::session_version(owner.account, &session.id, &version);
    let grants = |write| vec![PersonalVolumeGrant { volume, write }];
    store.select_personal_volumes(&session.id, grants(false)).unwrap();
    assert!(matches!(
        store.rename_entry_component(&access, volume, file.id, "denied.bin"),
        Err(Error::AccessDenied(_))
    ));
    store.select_personal_volumes(&session.id, grants(true)).unwrap();
    store.rename_entry_component(&access, volume, file.id, "allowed.bin").unwrap();
    store.select_personal_volumes(&session.id, grants(false)).unwrap();
    assert!(matches!(
        store.rename_entry_component(&access, volume, file.id, "withdrawn.bin"),
        Err(Error::AccessDenied(_))
    ));
    assert_eq!(store.resolve_entry(&owner, volume, "allowed.bin").unwrap().id, file.id);
}
