//! Exact-item Data Protection Keychain setup. Never enumerate or update items.
use super::{MARKER, Result};
use core_foundation::{
    base::{CFType, TCFType},
    boolean::CFBoolean,
    data::CFData,
    dictionary::CFDictionary,
    string::{CFString, CFStringRef},
};
use playbit::store::Id;
use security_framework_sys::{access_control::*, item::*, keychain_item::*};
use serde::Deserialize;
use std::{io::Read, path::Path, ptr};

#[allow(unsafe_code, reason = "Security.framework attribute constants use C linkage.")]
unsafe extern "C" {
    static kSecAttrGeneric: CFStringRef;
    static kSecAttrAccessible: CFStringRef;
    static kSecUseAuthenticationUIFail: CFStringRef;
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credential {
    account: String,
    service: String,
    secret: [u8; 16],
}

#[allow(
    unsafe_code,
    reason = "Synthetic fixture uses bounded synchronous Security.framework calls."
)]
pub(super) fn run(action: &str, path: &Path) -> Result<()> {
    let group = env!("PLAYBIT_KEYCHAIN_ACCESS_GROUP");
    if group.is_empty() {
        return Err("provisioned fixture requires a compiled Keychain access group".into());
    }
    let mut marker = String::new();
    std::fs::File::open(path.join(MARKER))?.take(64).read_to_string(&mut marker)?;
    let account = Id::parse(&marker)?.to_string();
    let service = "playbit.account_key".to_owned();
    // SAFETY: constants are process-lifetime values; owned CF objects remain alive
    // through each synchronous call. Every query specifies backend, group,
    // service, account and synchronizability; none can match unrelated credentials.
    let mut query: Vec<(CFString, CFType)> = unsafe {
        vec![
            (
                CFString::wrap_under_get_rule(kSecClass),
                CFString::wrap_under_get_rule(kSecClassGenericPassword).into_CFType(),
            ),
            (CFString::wrap_under_get_rule(kSecAttrService), CFString::new(&service).into_CFType()),
            (CFString::wrap_under_get_rule(kSecAttrAccount), CFString::new(&account).into_CFType()),
            (
                CFString::wrap_under_get_rule(kSecAttrAccessGroup),
                CFString::new(group).into_CFType(),
            ),
            (
                CFString::wrap_under_get_rule(kSecUseDataProtectionKeychain),
                CFBoolean::true_value().into_CFType(),
            ),
            (
                CFString::wrap_under_get_rule(kSecAttrSynchronizable),
                CFBoolean::true_value().into_CFType(),
            ),
            (
                CFString::wrap_under_get_rule(kSecUseAuthenticationUI),
                CFString::wrap_under_get_rule(kSecUseAuthenticationUIFail).into_CFType(),
            ),
        ]
    };
    let status = if action == "probe-key" {
        let attributes = CFDictionary::from_CFType_pairs(&query);
        unsafe { SecItemCopyMatching(attributes.as_concrete_TypeRef(), ptr::null_mut()) }
    } else if action == "delete-key" {
        let attributes = CFDictionary::from_CFType_pairs(&query);
        // SAFETY: the query above is fully scoped and valid for this call.
        unsafe { SecItemDelete(attributes.as_concrete_TypeRef()) }
    } else if action == "create-key" {
        let mut input = Vec::new();
        std::io::stdin().take(1025).read_to_end(&mut input)?;
        if input.len() > 1024 {
            return Err("fixture credential exceeds limit".into());
        }
        let credential: Credential = serde_json::from_slice(&input)?;
        if credential.account != account || credential.service != service {
            return Err("credential does not belong to this fixture".into());
        }
        // SAFETY: values outlive the synchronous call; Add rejects duplicates.
        unsafe {
            query.push((
                CFString::wrap_under_get_rule(kSecAttrGeneric),
                CFData::from_buffer(&[1]).into_CFType(),
            ));
            query.push((
                CFString::wrap_under_get_rule(kSecValueData),
                CFData::from_buffer(&credential.secret).into_CFType(),
            ));
            query.push((
                CFString::wrap_under_get_rule(kSecAttrAccessible),
                CFString::wrap_under_get_rule(kSecAttrAccessibleAfterFirstUnlock).into_CFType(),
            ));
            let attributes = CFDictionary::from_CFType_pairs(&query);
            SecItemAdd(attributes.as_concrete_TypeRef(), ptr::null_mut())
        }
    } else {
        return Err("unknown protected fixture action".into());
    };
    if status != 0 && !(action == "delete-key" && status == -25300) {
        return Err(format!("fixture Security.framework status {status}").into());
    }
    Ok(())
}
