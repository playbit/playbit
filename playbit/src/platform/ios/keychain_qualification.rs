//! Synthetic on-device backup fixture, absent from product builds.
use crate::{
    network::ap::ClientIdentity,
    store::{Id, LocalStore, RecoveryState},
};
use core_foundation::{base::TCFType, bundle::CFBundle, string::CFString};
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::Path};

const MARKER: &str = "control/keychain-qualification";

pub(super) fn prepare(profile: &Path) -> Result<bool, String> {
    let Ok(mode) = std::env::var("PLAYBIT_KEYCHAIN_QUALIFICATION") else { return Ok(false) };
    let info = CFBundle::main_bundle().info_dictionary();
    let app = info
        .find(CFString::new("CFBundleIdentifier"))
        .and_then(|value| value.downcast::<CFString>())
        .map(|value| value.to_string())
        .ok_or("qualification requires a bundle")?;
    if !app.starts_with("app.playbit.profiles.distribution.") || !crate::credentials::available() {
        return Err("Keychain qualification requires an isolated provisioned test bundle".into());
    }
    (|| -> crate::engine::Result<()> {
        let marker = profile.join(MARKER);
        if mode == "cleanup" {
            let account = Id::parse(&std::fs::read_to_string(&marker)?)?;
            remove(profile, account)?;
            return Ok(());
        }
        if mode != "seed" && mode != "fill" {
            return Err("invalid Keychain qualification mode".into());
        }
        if marker.exists() {
            return Ok(());
        }
        if profile.join(crate::paths::PROFILE_DB).exists() {
            return Err("Keychain qualification requires a fresh profile".into());
        }
        let store = LocalStore::open(profile)?;
        let account = store.account().id;
        if mode == "fill" {
            crate::credentials::remember_account(
                profile,
                account,
                "offline@example.invalid",
                &[42; 16],
            )?;
        } else {
            seed(profile, &store, account)?;
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(marker)?
            .write_all(account.to_string().as_bytes())?;
        Ok(())
    })()
    .map_err(|error| error.to_string())?;
    if mode == "cleanup" {
        // Stop before opening the worker so cleanup cannot schedule another backup.
        return Err("Qualification recovery copy removed".into());
    }
    Ok(true)
}

fn seed(profile: &Path, store: &LocalStore, account: Id) -> crate::engine::Result<()> {
    let mut identity = ClientIdentity::from_seed(
        "offline@example.invalid",
        "Keychain qualification",
        store.account_seed()?,
    )?;
    identity.auth_token = account.as_bytes().iter().copied().chain([0; 56]).collect();
    identity.instance_id = Id::from_bytes([1; 16]).to_string().into_bytes();
    identity.ca_certificate = vec![1];
    identity.instance_certificate = vec![1];
    identity.save(&profile.join("instance.json"))?;
    store.save_recovery_state(&RecoveryState {
        secret: [42; 16],
        acknowledged: true,
        revision: 1,
        synchronized_digest: None,
        pending: None,
    })?;
    store.set_display_name("Keychain qualification")?;
    Ok(())
}

fn remove(profile: &Path, account: Id) -> crate::engine::Result<()> {
    use core_foundation::{base::CFType, boolean::CFBoolean, dictionary::CFDictionary};
    use security_framework_sys::{item::*, keychain_item::*};
    let info = CFBundle::main_bundle().info_dictionary();
    let group = info
        .find(CFString::new("PlaybitKeychainAccessGroup"))
        .and_then(|value| value.downcast::<CFString>())
        .ok_or("missing qualification group")?;
    let service = crate::credentials::namespace::Namespace::for_profile(profile)?.service();
    // SAFETY: the app is an isolated qualification bundle. This query is bound
    // to its service, group and generated account, never a service-wide delete.
    let query = unsafe {
        CFDictionary::<CFString, CFType>::from_CFType_pairs(&[
            (
                CFString::wrap_under_get_rule(kSecClass),
                CFString::wrap_under_get_rule(kSecClassGenericPassword).into_CFType(),
            ),
            (CFString::wrap_under_get_rule(kSecAttrService), CFString::new(&service).into_CFType()),
            (
                CFString::wrap_under_get_rule(kSecAttrAccount),
                CFString::new(&account.to_string()).into_CFType(),
            ),
            (CFString::wrap_under_get_rule(kSecAttrAccessGroup), group.into_CFType()),
            (
                CFString::wrap_under_get_rule(kSecUseDataProtectionKeychain),
                CFBoolean::true_value().into_CFType(),
            ),
            (
                CFString::wrap_under_get_rule(kSecAttrSynchronizable),
                CFBoolean::true_value().into_CFType(),
            ),
        ])
    };
    let status = unsafe { SecItemDelete(query.as_concrete_TypeRef()) };
    if status != 0 && status != -25300 {
        return Err(format!("qualification cleanup failed: {status}").into());
    }
    let status = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), std::ptr::null_mut()) };
    if status != -25300 {
        return Err(format!("qualification item remains: {status}").into());
    }
    Ok(())
}
