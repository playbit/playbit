//! One durable selection and local creation intent. Retain the seed code before
//! creation so interrupted launches reopen the same lazily initialized session.
use crate::{
    engine::{Result, connected::Connected},
    package::Package,
    store::{Access, Id, Session},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};
const LIMIT: usize = 256;
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Selection {
    Creating { code: String, build: String },
    Session { id: String },
}
pub(super) fn read(profile: &Path) -> Result<Option<Selection>> {
    let file = match fs::File::open(profile.join("native-session.json")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > LIMIT {
        return Err("Native session selection exceeds its size limit".into());
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}
impl Selection {
    pub(super) fn write(&self, profile: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > LIMIT {
            return Err("Native session selection exceeds its size limit".into());
        }
        use std::os::unix::fs::OpenOptionsExt;
        let temporary =
            crate::paths::private_directory(profile, "staging")?.join("native-session.json");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(temporary, profile.join("native-session.json"))?;
        fs::File::open(profile)?.sync_all()?;
        Ok(())
    }
    pub(super) fn resume(self, connected: &Connected, package: &Package) -> Result<Session> {
        let build = package.id()?;
        let store = &connected.services.store;
        let creating = matches!(&self, Self::Creating { .. });
        let session = match self {
            Self::Session { id } => store.session(&id)?,
            Self::Creating { code, build: expected } => {
                if expected != build {
                    return Err("Finish creating the session with the original app build".into());
                }
                let seed = crate::store::parse_session_code(&code)?;
                connected.services.create_local_session(package, seed)?
            }
        };
        if session.resolved_build_id != build || &session.app != package.identity()? {
            return Err("This session uses a different app build. Run the matching native app or use a separate profile.".into());
        }
        let access = Access::session_version(store.account().id, &session.id, &build);
        if store.is_local_session(&session.id)? {
            store.admit_binding(&access, session.root_volume)?;
        } else {
            store.authorize_replication(
                &access,
                session.replica_volume(),
                crate::store::object_contract::Permission::Read,
            )?;
        }
        if creating {
            Self::Session { id: session.id.clone() }.write(connected.services.profile_path())?;
        }
        Ok(session)
    }
}
pub(super) fn create(connected: &Connected, package: &Package) -> Result<Session> {
    // A failed creation's intent must be resumed even if the user retries without
    // restarting. Its bounded local association is independent of account recovery.
    let intent = match read(connected.services.profile_path())? {
        Some(intent) => intent,
        None => {
            let seed = *Id::generate()?.as_bytes();
            let intent = Selection::Creating {
                code: crate::store::session_code(&seed),
                build: package.id()?,
            };
            intent.write(connected.services.profile_path())?;
            intent
        }
    };
    intent.resume(connected, package)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert durable fixture operations.")]
mod tests {
    use super::*;
    use crate::store::{
        EntryId, InitialDatabase, InitialDatabaseKind, InitialObject, RootInitialization,
        object_contract::{Schema, TypeDeclaration},
    };
    use std::collections::BTreeMap;

    fn declaration(type_id: Id) -> RootInitialization {
        RootInitialization {
            permissions: Default::default(),
            databases: vec![InitialDatabase {
                id: EntryId::from_bytes([5; 15]).expect("valid fixture entry ID"),
                name: "records".into(),
                kind: InitialDatabaseKind::ObjectDb {
                    schema: Schema {
                        types: BTreeMap::from([(
                            type_id,
                            TypeDeclaration {
                                name: "Item".into(),
                                deprecated: false,
                                fields: BTreeMap::new(),
                            },
                        )]),
                    },
                    objects: vec![InitialObject {
                        id: Id::from_bytes([8; 16]),
                        type_id,
                        fields: BTreeMap::new(),
                    }],
                },
            }],
            blobs: vec![],
        }
    }

    #[test]
    fn interrupted_creation_reuses_the_session_and_preserves_edits() {
        let directory = tempfile::tempdir().unwrap();
        let owner = Connected::open(directory.path(), || {}).unwrap();
        let type_id = Id::from_bytes([7; 16]);
        let declaration = declaration(type_id);
        let initializers = crate::package::VolumeInitializers::single(
            crate::package::AutomaticVolumeRole::Session,
            declaration.clone(),
        )
        .unwrap();
        let package = crate::package::signature::testing::signed(
            &owner.services.store,
            Package::create(
                "test".into(),
                "Test app".into(),
                b"\0asm\x01\0\0\0".to_vec(),
                Default::default(),
                &initializers,
            )
            .unwrap(),
        )
        .unwrap();
        let code = crate::store::session_code(&[0x47; 16]);
        let intent = Selection::Creating { code: code.clone(), build: package.id().unwrap() };
        intent.write(directory.path()).unwrap();
        let session = create(&owner, &package).unwrap();
        assert!(owner.services.store.is_local_session(&session.id).unwrap());
        assert_eq!(owner.services.store.session_reference(&session.id).unwrap().code(), code);
        for slot in owner.services.store.automatic_volumes(&session.id).unwrap() {
            assert_eq!(slot.state, crate::store::AutomaticVolumeState::Lazy);
        }
        let shared = owner
            .services
            .store
            .open_automatic_volume(&session.id, crate::package::AutomaticVolumeRole::Session)
            .unwrap();
        let access = Access::owner(owner.services.store.account().id);
        let database = crate::store::volume_index::EntryAddress {
            volume: shared.id,
            entry: declaration.databases[0].id,
        };
        owner.services.store.create_object(&access, database, type_id).unwrap();
        // Power loss after local creation but before selection publication.
        intent.write(directory.path()).unwrap();
        drop(owner);
        let mut owner = Connected::open(directory.path(), || {}).unwrap();
        let package = owner
            .authenticate_native_package(Package::from_bytes(&package.to_bytes().unwrap()).unwrap())
            .unwrap();
        let reopened = read(directory.path()).unwrap().unwrap().resume(&owner, &package).unwrap();
        assert_eq!(reopened, session);
        assert_eq!(owner.services.store.sessions().unwrap().len(), 1);
        assert_eq!(
            owner
                .services
                .store
                .query_objects(&access, database, type_id, None, 64)
                .unwrap()
                .objects
                .len(),
            2
        );
        let opened = owner.join_native(&code, &package).unwrap();
        assert_eq!(opened, session);
        let retained = fs::read(directory.path().join("native-session.json")).unwrap();
        let wrong = crate::package::signature::testing::signed(
            &owner.services.store,
            Package::create(
                "test".into(),
                "Different build".into(),
                package.wasm().unwrap(),
                Default::default(),
                &initializers,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(read(directory.path()).unwrap().unwrap().resume(&owner, &wrong).is_err());
        assert!(owner.join_native(&code, &wrong).is_err());
        assert_eq!(fs::read(directory.path().join("native-session.json")).unwrap(), retained);
        assert_eq!(owner.services.store.sessions().unwrap(), vec![session]);
    }
    #[test]
    fn malformed_selection_never_becomes_a_new_session() {
        // A corrupt or oversized local association is an explicit failure, not
        // an excuse to discard it and allocate a new data root.
        let directory = tempfile::tempdir().unwrap();
        assert!(read(directory.path()).unwrap().is_none());
        for bytes in [
            vec![b' '; LIMIT + 1],
            b"{}".to_vec(),
            b"{\"kind\":\"session\",\"id\":\"x\",\"unknown\":true}".to_vec(),
        ] {
            fs::write(directory.path().join("native-session.json"), &bytes).unwrap();
            assert!(read(directory.path()).is_err());
            assert_eq!(fs::read(directory.path().join("native-session.json")).unwrap(), bytes);
        }
    }
}
