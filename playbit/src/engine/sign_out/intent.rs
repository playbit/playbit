//! Bounded durable reset intent. Capture the selected invitation before removal,
//! then restore its capability into the fresh account before retiring the intent.
use super::*;
use crate::store::{InvitationPreview, LocalStore, Seed};
use std::io::{Read, Write};

// A bounded identity includes two certificates encoded as JSON byte arrays.
const MAX_BYTES: u64 = 256 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) enum Intent {
    Remove,
    Replace {
        seed: [u8; 32],
        identity: Box<crate::network::ap::ClientIdentity>,
        account_key: Option<[u8; 16]>,
    },
    Capture {
        session: String,
    },
    Restore {
        seed: Seed,
        paused: bool,
    },
}

impl Intent {
    pub(super) fn validate(&self) -> Result<()> {
        if let Self::Replace { seed, identity, .. } = self {
            identity.validate_preissued()?;
            identity.account_id()?;
            let expected = crate::network::ap::ClientIdentity::from_seed("", "", *seed)?;
            if identity.public_key()? != expected.public_key()? {
                return Err("Replacement credentials do not match the device key".into());
            }
        }
        Ok(())
    }
    pub(super) fn load_or_create(path: &Path, session: Option<&str>) -> Result<Self> {
        match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_BYTES {
                    return Err("Sign-out intent exceeds its size limit".into());
                }
                let intent: Self = serde_json::from_slice(&bytes)?;
                intent.validate()?;
                Ok(intent)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let intent = match session {
                    Some(session) => Self::Capture { session: session.into() },
                    None => Self::Remove,
                };
                intent.save(path)?;
                Ok(intent)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("Sign-out intent exceeds its size limit".into());
        }
        // The launcher lock serializes this stage file. Publish only synced,
        // complete bytes so interruption leaves the previous phase readable.
        let stage = path.with_extension("sign-out-stage");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&stage)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(stage, path)?;
        File::open(path.parent().expect("intent has a parent"))?.sync_all()?;
        Ok(())
    }

    pub(super) fn capture(&mut self, profile: &Path, path: &Path) -> Result<()> {
        if let Self::Capture { session } = self {
            let (seed, paused) = LocalStore::invitation_for_reset(profile, session)?;
            *self = Self::Restore { seed, paused };
            self.save(path)?;
        }
        Ok(())
    }

    pub(super) fn restore(&self, profile: &Path, lock: &File) -> Result<Option<InvitationPreview>> {
        match self {
            Self::Remove => Ok(None),
            Self::Replace { seed, identity, account_key } => {
                let account = crate::store::Id::from_bytes(identity.account_id()?);
                let store = LocalStore::open_replacement(profile, lock, account, *seed)?;
                if store.account().id != account || store.account_seed()? != *seed {
                    return Err(
                        "Replacement profile identity does not match confirmed sign-in".into()
                    );
                }
                if let Some(key) = account_key {
                    store.protect_account_key(account, key)?;
                }
                identity.save(&profile.join("instance.json"))?;
                Ok(None)
            }
            Self::Capture { .. } => {
                Err("Invitation must be captured before profile removal".into())
            }
            Self::Restore { seed, paused } => {
                let channel = crate::engine::profile::for_profile(profile)?;
                let store = LocalStore::open_for_reset(profile, lock)?;
                let preview = store
                    .restore_invitation_seed(&crate::store::session_link(seed, channel), channel)?;
                store.pause_invitation(&preview.session, *paused)?;
                Ok(Some(store.pending_invitation_preview(&preview.session)?))
            }
        }
    }
}
