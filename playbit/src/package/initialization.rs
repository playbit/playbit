//! Signed role declarations contain schemas and seed data, never chosen owners,
//! scope identities or private-reader grants. An explicit Session declaration
//! selects Shared for a fresh native session's primary backing.
use super::{AutomaticVolumeRole, Result};
use crate::store::{
    RootInitialization,
    object_contract::{Acl, Grant, Permission, Subject},
};
use serde::Serialize;
use std::collections::BTreeMap;

pub const MAX_INITIALIZERS_BYTES: usize = 6 * crate::store::MAX_ROOT_INITIALIZATION_BYTES;

#[derive(Clone, Debug, Default, Serialize)]
#[serde(transparent)]
pub struct VolumeInitializers {
    roles: BTreeMap<AutomaticVolumeRole, RootInitialization>,
}

impl VolumeInitializers {
    pub fn new(roles: BTreeMap<AutomaticVolumeRole, RootInitialization>) -> Result<Self> {
        for (role, declaration) in &roles {
            declaration.fingerprint()?;
            if *role != AutomaticVolumeRole::Root && !declaration.permissions.grants.is_empty() {
                return Err(
                    "automatic private/shared initializers cannot declare reader or writer grants"
                        .into(),
                );
            }
        }
        let value = Self { roles };
        value.to_json()?;
        Ok(value)
    }
    pub fn single(role: AutomaticVolumeRole, declaration: RootInitialization) -> Result<Self> {
        Self::new(BTreeMap::from([(role, declaration)]))
    }
    // fuzz: package_contract
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let value = super::json::parse(bytes, MAX_INITIALIZERS_BYTES)?;
        Self::new(serde_json::from_value(value)?)
    }
    pub fn to_json(&self) -> Result<Vec<u8>> {
        super::json::encode(self, MAX_INITIALIZERS_BYTES)
    }
    /// Presence distinguishes an explicitly empty scope from an omitted role.
    pub fn declares(&self, role: AutomaticVolumeRole) -> bool {
        self.roles.contains_key(&role)
    }
    pub fn for_role(&self, role: AutomaticVolumeRole) -> RootInitialization {
        self.roles.get(&role).cloned().unwrap_or_else(|| RootInitialization {
            permissions: if role == AutomaticVolumeRole::Root {
                Acl {
                    grants: vec![Grant {
                        subject: Subject::Public,
                        permissions: vec![Permission::Read],
                    }],
                }
            } else {
                Acl::default()
            },
            databases: Vec::new(),
            blobs: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_roles_are_empty_and_world_defaults_to_public_read_only() {
        // Merely resolving a declaration does not allocate a store or create data.
        let initializers = VolumeInitializers::from_json(b"{}").expect("empty role map");
        assert!(!initializers.declares(AutomaticVolumeRole::Session));
        let explicit = VolumeInitializers::single(
            AutomaticVolumeRole::Session,
            initializers.for_role(AutomaticVolumeRole::Session),
        )
        .expect("explicitly empty Shared declaration");
        assert!(explicit.declares(AutomaticVolumeRole::Session));
        assert!(
            VolumeInitializers::from_json(&explicit.to_json().expect("canonical declaration"))
                .expect("round trip")
                .declares(AutomaticVolumeRole::Session)
        );
        let root = initializers.for_role(AutomaticVolumeRole::Root);
        assert_eq!(root.permissions.grants.len(), 1);
        assert_eq!(root.permissions.grants[0].subject, Subject::Public);
        assert_eq!(root.permissions.grants[0].permissions, [Permission::Read]);
        for role in [
            AutomaticVolumeRole::Session,
            AutomaticVolumeRole::User,
            AutomaticVolumeRole::UserSession,
            AutomaticVolumeRole::Local,
            AutomaticVolumeRole::LocalSession,
        ] {
            let declaration = initializers.for_role(role);
            assert!(declaration.permissions.grants.is_empty());
            assert!(declaration.databases.is_empty());
            assert!(declaration.blobs.is_empty());
        }
    }
    #[test]
    fn role_names_and_private_permission_ceilings_are_closed() {
        // Reject broadening a private/shared role before any initialization is admitted.
        let public = r#"{"permissions":{"grants":[{"subject":{"kind":"public"},"permissions":["read"]}]},"databases":[],"blobs":[]}"#;
        assert!(
            VolumeInitializers::from_json(format!(r#"{{"root":{public}}}"#).as_bytes()).is_ok()
        );
        for role in ["session", "user", "user-session", "local", "local-session"] {
            assert!(
                VolumeInitializers::from_json(format!(r#"{{"{role}":{public}}}"#).as_bytes())
                    .is_err()
            );
        }
        for bytes in [
            b"{\"unknown\":{}}".as_slice(),
            b"{\"root\":{},\"root\":{}}",
            b"{\"user\":{\"owner\":\"untrusted\"}}",
        ] {
            assert!(VolumeInitializers::from_json(bytes).is_err());
        }
    }

    #[test]
    fn entry_initializers_cannot_override_the_scope_acl() {
        // Entry declarations inherit the host's scope policy; an embedded ACL
        // must fail parsing rather than introduce additional readers.
        let mut declaration = serde_json::json!({
            "user": {
                "permissions": {"grants": []},
                "databases": [{
                    "id": crate::identifier::encode(&[1; 15]),
                    "name": "data",
                    "kind": {"type": "kv_db"}
                }],
                "blobs": []
            }
        });
        assert!(
            VolumeInitializers::from_json(&serde_json::to_vec(&declaration).expect("fixture JSON"))
                .is_ok()
        );
        declaration["user"]["databases"][0]["permissions"] = serde_json::json!({
            "grants": [{"subject": {"kind": "public"}, "permissions": ["read"]}]
        });
        assert!(
            VolumeInitializers::from_json(&serde_json::to_vec(&declaration).expect("fixture JSON"))
                .is_err()
        );
    }
}
