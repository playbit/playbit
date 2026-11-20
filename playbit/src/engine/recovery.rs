//! Account sign-in authorizes AP access. Saved secrets separately unlock the
//! credential catalog; authentication keys never enter this backup.
// W7 coverage decision: recovery's control/network owner presents failure and
// retains durable pending-upload identity. Tests reject malformed/unauthorized
// vaults and mismatched acknowledgments. Per-call RNG/store failures are not
// separately injected; never substitute an empty vault or undo durable imports.
#[cfg(feature = "app-host")]
pub(crate) mod control;

use crate::engine::{
    Result,
    reply::{PreparedRecovery, RecoveryStatus},
};
use crate::store::{Id, RecoveryState, RecoveryUpload, recovery};
use serde::Deserialize;
use serde_json::Value;
// Runs on the enrollment worker. Completion can publish the prepared state
// without making a Keychain call on the engine's input/event loop.
pub(crate) fn stage_recovery_key(
    store: &crate::store::LocalStore,
    account: Id,
    vault: &mut Vault,
) -> Result<()> {
    if vault.revision != 0 {
        return Ok(());
    }
    let secret = match store.recovery_state()? {
        Some(state) if store.account().id == account => state.secret,
        _ => match store.prepared_account_key(account)? {
            Some(key) => key,
            None => crate::store::account_key::generate()?,
        },
    };
    store.protect_account_key(account, &secret)?;
    vault.prepared_secret = Some(secret);
    Ok(())
}

#[cfg(test)]
use serde_json::json;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Vault {
    revision: u64,
    ciphertext: Option<String>,
    #[serde(skip)]
    pub(crate) prepared_secret: Option<[u8; 16]>,
}
impl Vault {
    pub(crate) fn parse(value: Value) -> Result<Self> {
        let vault: Self = serde_json::from_value(value)?;
        if (vault.revision == 0) != vault.ciphertext.is_none() {
            return Err("invalid credential backup revision".into());
        }
        if let Some(ciphertext) = &vault.ciphertext {
            recovery::decode_ciphertext(ciphertext)?;
        }
        Ok(vault)
    }
    fn decrypt(&self, account: Id, secret: &[u8; 16]) -> Result<Vec<u8>> {
        let ciphertext =
            self.ciphertext.as_ref().ok_or("no credential backup exists for this account")?;
        Ok(recovery::open(
            account,
            self.revision,
            &crate::store::account_key::backup_key(account, secret)?,
            &recovery::decode_ciphertext(ciphertext)?,
        )?)
    }
}

pub(crate) fn local_status(store: &crate::store::LocalStore) -> Result<Option<RecoveryStatus>> {
    let Some(state) = store.recovery_state()?.filter(|s| s.acknowledged) else {
        return Ok(None);
    };
    Ok(Some(RecoveryStatus::Ready {
        revision: state.revision,
        pending_save: state.pending.is_some() || state.revision == 0,
        restored_sessions: None,
        restored_volumes: None,
    }))
}

pub(super) fn status(
    remote: &mut crate::engine::remote::Remote,
    store: &crate::store::LocalStore,
) -> Result<RecoveryStatus> {
    if let Some(state) = local_status(store)? {
        return Ok(state);
    }
    let state = store.recovery_state()?;
    let vault = remote.credential_vault(store)?;
    Ok(if vault.revision == 0 {
        RecoveryStatus::Setup { revision: 0, pending_save: state.is_some(), secret: None }
    } else {
        RecoveryStatus::Locked { revision: vault.revision, pending_save: false }
    })
}

pub(super) fn prepare(
    remote: &mut crate::engine::remote::Remote,
    store: &crate::store::LocalStore,
) -> Result<PreparedRecovery> {
    if !matches!(status(remote, store)?, RecoveryStatus::Setup { .. }) {
        return Err(
            "recover existing credentials instead of replacing this account’s account key".into()
        );
    }
    prepare_local(store, None)
}

fn prepare_local(
    store: &crate::store::LocalStore,
    prepared_secret: Option<[u8; 16]>,
) -> Result<PreparedRecovery> {
    let state = match store.recovery_state()? {
        Some(state) => state,
        None => RecoveryState {
            secret: match prepared_secret {
                Some(secret) => secret,
                None => match store.prepared_account_key(store.account().id)? {
                    Some(key) => key,
                    None => crate::store::account_key::generate()?,
                },
            },
            acknowledged: false,
            revision: 0,
            synchronized_digest: None,
            pending: None,
        },
    };
    store.save_recovery_state(&state)?;
    Ok(PreparedRecovery { secret: crate::store::account_key::format(&state.secret) })
}

/// Enrollment already authenticated this vault read. Return setup with its
/// durable, unacknowledged key before synchronization starts. Saving still
/// rechecks the AP and atomically registers the verifier with the backup.
pub(crate) fn after_sign_in(
    store: &crate::store::LocalStore,
    vault: Vault,
) -> Result<RecoveryStatus> {
    if let Some(state) = local_status(store)? {
        return Ok(state);
    }
    if vault.revision != 0 {
        return Ok(RecoveryStatus::Locked { revision: vault.revision, pending_save: false });
    }
    let prepared = prepare_local(store, vault.prepared_secret)?;
    Ok(RecoveryStatus::Setup { revision: 0, pending_save: true, secret: Some(prepared.secret) })
}

pub(super) fn save(
    remote: &mut crate::engine::remote::Remote,
    store: &crate::store::LocalStore,
) -> Result<RecoveryStatus> {
    let mut state = store.recovery_state()?.ok_or("prepare and save your account key first")?;
    if !state.acknowledged {
        if remote.credential_vault(store)?.revision != 0 {
            return Err("this account already has a backup; use its saved account key".into());
        }
        // Only this explicit request acknowledges the save. Preparing or
        // copying the secret must not claim the user retained it.
        state.acknowledged = true;
        store.save_recovery_state(&state)?;
    }
    backup_credentials(remote, store)?;
    status(remote, store)
}

/// All network and durable catalog work can run with the existing background
/// Remote owner. Installing the resulting session bookkeeping stays on Engine.
pub(crate) fn restore_credentials(
    remote: &mut crate::engine::remote::Remote,
    store: &crate::store::LocalStore,
    secret: &[u8; 16],
    canceled: Option<&crate::engine::cancellation::Canceled>,
) -> Result<RecoveryStatus> {
    if !remote.enrolled() {
        return Err("sign in before recovering private data".into());
    }
    let is_canceled = || canceled.is_some_and(crate::engine::cancellation::Canceled::raised);
    let vault = remote.credential_vault(store);
    // An intentional transport shutdown is cancellation, not a failed import.
    if is_canceled() {
        return Ok(RecoveryStatus::Canceled);
    }
    let vault = vault?;
    let catalog = vault.decrypt(store.account().id, secret)?;
    store.protect_account_key(store.account().id, secret)?;
    let Some((sessions, volumes)) = store.import_recovery_catalog(&catalog, || !is_canceled())?
    else {
        return Ok(RecoveryStatus::Canceled);
    };
    let local = store.recovery_catalog()?;
    let synchronized_digest =
        if serde_json::from_slice::<Value>(&catalog)? == serde_json::from_slice::<Value>(&local)? {
            Some(blake3::hash(&local).to_hex().to_string())
        } else {
            None
        };
    store.save_recovery_state(&RecoveryState {
        secret: *secret,
        acknowledged: true,
        revision: vault.revision,
        synchronized_digest,
        pending: None,
    })?;
    Ok(RecoveryStatus::Ready {
        revision: vault.revision,
        pending_save: false,
        restored_sessions: Some(sessions),
        restored_volumes: Some(volumes),
    })
}

pub(crate) fn backup_credentials(
    remote: &mut crate::engine::remote::Remote,
    store: &crate::store::LocalStore,
) -> Result<()> {
    let Some(mut state) = store.recovery_state()?.filter(|s| s.acknowledged) else {
        return Ok(());
    };
    let local = store.recovery_catalog()?;
    let digest = blake3::hash(&local).to_hex().to_string();
    if state.pending.is_none() && state.synchronized_digest.as_ref() == Some(&digest) {
        return Ok(());
    }
    let mut latest = None;
    if let Some(upload) = state.pending.clone() {
        match remote.put_credential_vault(store, &upload) {
            Ok(vault) => {
                acknowledge_recovery(store, &mut state, &upload, vault)?;
                if upload.local_digest == digest {
                    return Ok(());
                }
            }
            Err(error) => {
                let vault = remote.credential_vault(store)?;
                if vault.revision <= upload.expected_revision {
                    return Err(error);
                }
                latest = Some(vault);
            }
        }
    }
    let vault = match latest {
        Some(value) => value,
        None => remote.credential_vault(store)?,
    };
    if vault.revision < state.revision {
        return Err("credential backup is older than this device’s acknowledgment".into());
    }
    let merged = if vault.revision == 0 {
        local
    } else {
        match vault.decrypt(store.account().id, &state.secret) {
            Ok(remote) => store.merge_recovery_catalog(&remote)?,
            Err(error) => {
                if state.revision == 0 {
                    // Another device completed setup first. Never overwrite
                    // its backup with a secret this device just generated.
                    state.acknowledged = false;
                    state.pending = None;
                    store.save_recovery_state(&state)?;
                }
                return Err(error);
            }
        }
    };
    let revision = vault.revision.checked_add(1).ok_or("credential backup revision exhausted")?;
    let ciphertext = recovery::encode_ciphertext(&recovery::seal(
        store.account().id,
        revision,
        &crate::store::account_key::backup_key(store.account().id, &state.secret)?,
        &merged,
    )?)?;
    let upload = RecoveryUpload {
        operation_id: Id::generate()?,
        expected_revision: vault.revision,
        ciphertext,
        local_digest: digest,
    };
    state.pending = Some(upload.clone());
    store.save_recovery_state(&state)?;
    let result = remote.put_credential_vault(store, &upload)?;
    acknowledge_recovery(store, &mut state, &upload, result)?;
    Ok(())
}

fn acknowledge_recovery(
    store: &crate::store::LocalStore,
    state: &mut RecoveryState,
    upload: &RecoveryUpload,
    vault: Vault,
) -> Result<()> {
    if Some(vault.revision) != upload.expected_revision.checked_add(1)
        || vault.ciphertext.as_deref() != Some(upload.ciphertext.as_str())
    {
        return Err("credential backup acknowledgment does not match pending upload".into());
    }
    state.revision = vault.revision;
    state.synchronized_digest = Some(upload.local_digest.clone());
    state.pending = None;
    store.save_recovery_state(state)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn enrollment_prepares_a_durable_key_without_acknowledging_or_replacing_a_backup() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::LocalStore::open(directory.path()).unwrap();
        let setup =
            after_sign_in(&store, Vault { revision: 0, ciphertext: None, prepared_secret: None })
                .unwrap();
        let RecoveryStatus::Setup { secret: Some(key), .. } = &setup else {
            panic!("expected setup key")
        };
        assert_eq!(
            crate::store::account_key::parse(key).unwrap(),
            store.recovery_state().unwrap().unwrap().secret
        );
        assert!(!store.recovery_state().unwrap().unwrap().acknowledged);
        assert!(local_status(&store).unwrap().is_none(), "preparation is not unlocked recovery");
        assert_eq!(
            setup,
            after_sign_in(&store, Vault { revision: 0, ciphertext: None, prepared_secret: None })
                .unwrap()
        );
        let locked = after_sign_in(
            &store,
            Vault { revision: 1, ciphertext: Some("opaque".into()), prepared_secret: None },
        )
        .unwrap();
        assert!(matches!(locked, RecoveryStatus::Locked { revision: 1, pending_save: false }));
        assert_eq!(
            crate::store::account_key::format(&store.recovery_state().unwrap().unwrap().secret),
            key.as_str()
        );
        drop(store);
        let reopened = crate::store::LocalStore::open(directory.path()).unwrap();
        assert_eq!(
            crate::store::account_key::format(&reopened.recovery_state().unwrap().unwrap().secret),
            key.as_str()
        );
        assert!(!reopened.recovery_state().unwrap().unwrap().acknowledged);
    }

    #[test]
    fn vault_errors_never_turn_malformed_or_unauthorized_backup_into_empty_state() {
        for value in [
            json!({"revision": 1}),
            json!({"revision": 0, "ciphertext": "invalid"}),
            json!({"revision": 1, "ciphertext": "invalid"}),
            json!({"revision": 0, "unknown": true}),
        ] {
            assert!(Vault::parse(value).is_err());
        }
        let account = Id::from_bytes([1; 16]);
        let absent = Vault::parse(json!({"revision": 0})).unwrap();
        assert!(absent.decrypt(account, &[7; 16]).is_err());
        let ciphertext = recovery::encode_ciphertext(
            &recovery::seal(
                account,
                1,
                &crate::store::account_key::backup_key(account, &[7; 16]).unwrap(),
                b"retained credentials",
            )
            .unwrap(),
        )
        .unwrap();
        let vault = Vault::parse(json!({"revision": 1, "ciphertext": ciphertext})).unwrap();
        assert!(vault.decrypt(account, &[8; 16]).is_err());
        assert!(vault.decrypt(Id::from_bytes([2; 16]), &[7; 16]).is_err());
        assert_eq!(vault.decrypt(account, &[7; 16]).unwrap(), b"retained credentials");
    }

    #[test]
    fn mismatched_vault_acknowledgment_keeps_durable_pending_upload() {
        let directory = tempfile::tempdir().unwrap();
        let services = crate::engine::Services::open(directory.path(), || {}).unwrap();
        let upload = RecoveryUpload {
            operation_id: Id::from_bytes([3; 16]),
            expected_revision: 4,
            ciphertext: "pending ciphertext".into(),
            local_digest: "pending digest".into(),
        };
        let mut state = RecoveryState {
            secret: [7; 16],
            acknowledged: true,
            revision: 4,
            synchronized_digest: None,
            pending: Some(upload.clone()),
        };
        services.store.save_recovery_state(&state).unwrap();
        for vault in [
            Vault {
                revision: 4,
                ciphertext: Some(upload.ciphertext.clone()),
                prepared_secret: None,
            },
            Vault {
                revision: 5,
                ciphertext: Some("different ciphertext".into()),
                prepared_secret: None,
            },
        ] {
            assert!(acknowledge_recovery(&services.store, &mut state, &upload, vault).is_err());
            assert_eq!(state.revision, 4);
            assert!(state.pending.is_some());
            let retained = services.store.recovery_state().unwrap().unwrap();
            assert_eq!(retained.revision, 4);
            assert_eq!(retained.pending.unwrap().operation_id, upload.operation_id);
        }
    }
}
