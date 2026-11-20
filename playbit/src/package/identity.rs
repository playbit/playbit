//! Application identity is publishing provenance, independent of sessions and builds.
use crate::store::Id;
use serde::{Deserialize, Serialize};

pub const MAX_APP_ID_BYTES: usize = 64;
const VOLUME_ID_DOMAIN: &str = "app.playbit.automatic-volume.id.v1";

/// Permanent role tags used in automatic volume identities, independent of UI labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum AutomaticVolumeRole {
    Root = 1,
    Session = 2,
    User = 3,
    UserSession = 4,
    Local = 5,
    LocalSession = 6,
}

/// Anonymous local state occupies a separate namespace and never identifies an AP account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum LocalPrincipal {
    Account(Id),
    Anonymous(Id),
}

impl AutomaticVolumeRole {
    pub fn is_session(self) -> bool {
        matches!(self, Self::Session | Self::UserSession | Self::LocalSession)
    }
    pub fn is_local(self) -> bool {
        matches!(self, Self::Local | Self::LocalSession)
    }
    pub fn has_account(self) -> bool {
        matches!(self, Self::User | Self::UserSession | Self::Local | Self::LocalSession)
    }
}

/// A namespace changes the signed app ID. An explicitly named ID has the same identity.
pub fn effective_app_id(app_id: &str, namespace: Option<&str>) -> Result<String, &'static str> {
    if !valid_app_id(app_id) || namespace.is_some_and(|value| !valid_app_id(value)) {
        return Err("invalid app ID or namespace");
    }
    let effective =
        namespace.map_or_else(|| app_id.to_owned(), |value| format!("{app_id}.{value}"));
    if !valid_app_id(&effective) {
        return Err("effective app ID exceeds 64 bytes");
    }
    Ok(effective)
}

pub fn valid_app_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_APP_ID_BYTES
        && value.split(['.', '-']).all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct AppIdentity {
    pub publisher: Id,
    pub app_id: String,
}

impl AppIdentity {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.publisher == Id::from_bytes([0; 16]) || !valid_app_id(&self.app_id) {
            return Err("invalid application identity");
        }
        Ok(())
    }

    pub fn update_channel_id(&self) -> Result<[u8; 32], &'static str> {
        self.validate()?;
        let mut hash = blake3::Hasher::new();
        hash.update(b"app.playbit.app-updates.v1\0");
        hash.update(self.publisher.as_bytes());
        hash.update(&(self.app_id.len() as u32).to_le_bytes());
        hash.update(self.app_id.as_bytes());
        Ok(*hash.finalize().as_bytes())
    }

    /// Domain-separated address derivation. IDs are public addresses, never decryption keys.
    pub fn volume_id(
        &self,
        role: AutomaticVolumeRole,
        session: Option<[u8; 32]>,
        account: Option<Id>,
        profile: Option<Id>,
    ) -> Result<Id, &'static str> {
        self.derive_volume_id(role, session, account, profile, 0)
    }

    pub fn local_volume_id(
        &self,
        role: AutomaticVolumeRole,
        session: Option<[u8; 32]>,
        principal: LocalPrincipal,
        profile: Id,
    ) -> Result<Id, &'static str> {
        if !role.is_local() {
            return Err("local principal requires a local volume role");
        }
        let (kind, id) = match principal {
            LocalPrincipal::Account(id) => (0, id),
            LocalPrincipal::Anonymous(id) => (1, id),
        };
        self.derive_volume_id(role, session, Some(id), Some(profile), kind)
    }

    fn derive_volume_id(
        &self,
        role: AutomaticVolumeRole,
        session: Option<[u8; 32]>,
        account: Option<Id>,
        profile: Option<Id>,
        local_kind: u8,
    ) -> Result<Id, &'static str> {
        self.validate()?;
        if session.is_some() != role.is_session()
            || account.is_some() != role.has_account()
            || profile.is_some() != role.is_local()
            || session.is_some_and(|value| value == [0; 32])
            || account.is_some_and(|value| value == Id::from_bytes([0; 16]))
            || profile.is_some_and(|value| value == Id::from_bytes([0; 16]))
        {
            return Err("automatic volume derivation has invalid role inputs");
        }
        let mut hash = blake3::Hasher::new_derive_key(VOLUME_ID_DOMAIN);
        hash.update(&[role as u8]);
        hash.update(self.publisher.as_bytes());
        hash.update(&[self.app_id.len() as u8]);
        hash.update(self.app_id.as_bytes());
        if let Some(session) = session {
            hash.update(&session);
        }
        if let Some(account) = account {
            if role.is_local() {
                hash.update(&[local_kind]);
            }
            hash.update(account.as_bytes());
        }
        if let Some(profile) = profile {
            hash.update(profile.as_bytes());
        }
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&hash.finalize().as_bytes()[..16]);
        // AP reserves this address namespace for authenticated automatic creation.
        bytes[0] |= 0x80;
        Ok(Id::from_bytes(bytes))
    }

    /// An OS namespace suffix, not an alternative serialization of account IDs.
    pub fn short_bundle_id(&self) -> String {
        const ALPHABET: &[u8; 32] = b"0123456789abcdefghijklmnopqrstuv";
        let mut suffix = String::with_capacity(16);
        let mut accumulator = 0u32;
        let mut bits = 0;
        for byte in &self.publisher.as_bytes()[..10] {
            accumulator = (accumulator << 8) | u32::from(*byte);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                suffix.push(ALPHABET[((accumulator >> bits) & 31) as usize] as char);
            }
        }
        format!("{}.{suffix}", self.app_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_ids_and_os_namespace_have_one_shape() {
        for value in ["pinball", "pinball.testing", "a-2.b", "0"] {
            assert!(valid_app_id(value));
        }
        for value in ["", "Pinball", ".a", "a-", "a..b", "a.-b", "../app", "a_b"] {
            assert!(!valid_app_id(value));
        }
        assert!(!valid_app_id(&"a".repeat(65)));
        let identity =
            AppIdentity { publisher: Id::from_bytes([255; 16]), app_id: "pinball".into() };
        assert_eq!(identity.short_bundle_id(), "pinball.vvvvvvvvvvvvvvvv");
        let mut other = identity.clone();
        other.publisher = Id::from_bytes([254; 16]);
        assert_ne!(identity.short_bundle_id(), other.short_bundle_id());
    }

    #[test]
    fn namespace_is_part_of_the_effective_signed_identity() {
        // Namespace syntax has no hidden identity dimension or punctuation aliases.
        assert_eq!(effective_app_id("pinball", None), Ok("pinball".into()));
        assert_eq!(effective_app_id("pinball", Some("dev")), Ok("pinball.dev".into()));
        assert_eq!(effective_app_id("pinball.dev", None), effective_app_id("pinball", Some("dev")));
        for namespace in ["", "DEV", "../dev", ".dev", "dev_1"] {
            assert!(effective_app_id("pinball", Some(namespace)).is_err());
        }
        assert!(effective_app_id(&"a".repeat(64), Some("dev")).is_err());
    }

    #[test]
    fn automatic_volume_roles_have_exact_inputs_and_distinct_stable_addresses() {
        // Independent b3sum vectors freeze binary field order and domain separation.
        let app = AppIdentity { publisher: Id::from_bytes([2; 16]), app_id: "pinball".into() };
        let roles = [
            AutomaticVolumeRole::Root,
            AutomaticVolumeRole::Session,
            AutomaticVolumeRole::User,
            AutomaticVolumeRole::UserSession,
            AutomaticVolumeRole::Local,
            AutomaticVolumeRole::LocalSession,
        ];
        let mut ids = std::collections::BTreeSet::new();
        let vectors = [
            "Wbq7MEg1vfM5VWqTSRvCyA",
            "TybyDk2532XVqtyV8TJYfR",
            "S745nfXiust4Wakn7w2QGi",
            "R1XbjRPsQDj54CtWwZntpo",
            "XpXT4BkdBTptKWv7EnXUjH",
            "TnoSZwCy6sxCVD9xquCFSi",
        ];
        for role in roles {
            let session = role.is_session().then_some([3; 32]);
            let account = role.has_account().then_some(Id::from_bytes([4; 16]));
            let profile = role.is_local().then_some(Id::from_bytes([5; 16]));
            let id = app.volume_id(role, session, account, profile).expect("valid derivation");
            assert_eq!(id.to_string(), vectors[role as usize - 1]);
            assert_ne!(id.as_bytes()[0] & 0x80, 0);
            assert!(ids.insert(id));
            assert_eq!(app.volume_id(role, session, account, profile), Ok(id));
            assert!(app.volume_id(role, session, account, Some(Id::from_bytes([0; 16]))).is_err());
            if role.is_session() {
                assert_ne!(app.volume_id(role, Some([6; 32]), account, profile), Ok(id));
                assert!(app.volume_id(role, None, account, profile).is_err());
            } else {
                assert!(app.volume_id(role, Some([3; 32]), account, profile).is_err());
            }
            if role.is_local() {
                let account = Id::from_bytes([4; 16]);
                let profile = Id::from_bytes([5; 16]);
                assert_eq!(
                    app.local_volume_id(role, session, LocalPrincipal::Account(account), profile),
                    Ok(id)
                );
                let anonymous = app
                    .local_volume_id(role, session, LocalPrincipal::Anonymous(account), profile)
                    .expect("valid anonymous local derivation");
                assert_ne!(anonymous, id);
                assert_eq!(
                    anonymous.to_string(),
                    if role == AutomaticVolumeRole::Local {
                        "KQmTzwY7WWNGP77ZbWtYgL"
                    } else {
                        "Jh6uaPiuXvmdz2GHj52of1"
                    }
                );
            }
            let other = AppIdentity { publisher: Id::from_bytes([7; 16]), ..app.clone() };
            assert_ne!(other.volume_id(role, session, account, profile), Ok(id));
        }
    }
}
