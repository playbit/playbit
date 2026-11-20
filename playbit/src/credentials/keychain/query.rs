//! SecItem queries shared by macOS and iOS; backend selection never follows an error.
#![allow(
    unsafe_code,
    reason = "Security.framework exposes SecItem and attribute constants through C FFI."
)]
use core_foundation::{
    base::{CFType, TCFType},
    boolean::CFBoolean,
    data::CFData,
    dictionary::CFDictionary,
    string::{CFString, CFStringRef},
};
use security_framework_sys::{access_control::*, item::*, keychain_item::*};
use std::{io, ptr};

#[cfg(any(test, feature = "connected-engine"))]
use core_foundation::{array::CFArray, number::CFNumber};

type Attributes = Vec<(CFString, CFType)>;
unsafe extern "C" {
    static kSecAttrGeneric: CFStringRef;
    static kSecAttrAccessible: CFStringRef;
    static kSecUseAuthenticationUIFail: CFStringRef;
}

struct Policy {
    group: Option<String>,
}
impl Policy {
    #[cfg(all(target_os = "macos", feature = "process"))]
    fn current() -> io::Result<Self> {
        // Packaging derives this constant and the engine's entitlements from
        // the same provisioning profile. No runtime plist or environment lookup.
        let group = env!("PLAYBIT_KEYCHAIN_ACCESS_GROUP");
        Ok(Self { group: (!group.is_empty()).then(|| group.to_owned()) })
    }
    #[cfg(not(all(target_os = "macos", feature = "process")))]
    fn current() -> io::Result<Self> {
        let info = core_foundation::bundle::CFBundle::main_bundle().info_dictionary();
        let value = info.find(CFString::new("PlaybitKeychainAccessGroup"));
        let group = match value {
            None => None,
            Some(value) => {
                let text = value
                    .downcast::<CFString>()
                    .ok_or_else(|| io::Error::other("invalid Keychain access group configuration"))?
                    .to_string();
                if text.is_empty() {
                    None
                } else {
                    if text.len() > 255
                        || !text
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
                    {
                        return Err(io::Error::other(
                            "unexpanded or invalid Keychain access group",
                        ));
                    }
                    Some(text)
                }
            }
        };
        Ok(Self { group })
    }
    fn query(&self, service: &str, account: Option<&str>) -> io::Result<Attributes> {
        let group = self.group.as_ref().ok_or_else(|| {
            io::Error::other("Saved account recovery is unavailable in this build")
        })?;
        let mut attributes = vec![];
        // SAFETY: all keys/constant values are process-lifetime Security constants;
        // owned Core Foundation values outlive each synchronous SecItem call.
        unsafe {
            put(
                &mut attributes,
                kSecClass,
                CFString::wrap_under_get_rule(kSecClassGenericPassword),
            );
            put(&mut attributes, kSecAttrService, CFString::new(service));
            if let Some(account) = account {
                put(&mut attributes, kSecAttrAccount, CFString::new(account));
            }
            put(&mut attributes, kSecUseDataProtectionKeychain, CFBoolean::true_value());
            put(&mut attributes, kSecAttrSynchronizable, CFBoolean::true_value());
            put(
                &mut attributes,
                kSecUseAuthenticationUI,
                CFString::wrap_under_get_rule(kSecUseAuthenticationUIFail),
            );
            put(&mut attributes, kSecAttrAccessGroup, CFString::new(group));
        }
        Ok(attributes)
    }
    fn accessibility(&self, attributes: &mut Attributes) {
        unsafe {
            put(
                attributes,
                kSecAttrAccessible,
                CFString::wrap_under_get_rule(kSecAttrAccessibleAfterFirstUnlock),
            );
        }
    }
}

#[cfg(not(test))]
pub(super) fn available() -> bool {
    Policy::current().is_ok_and(|policy| policy.group.is_some())
}

unsafe fn put(attributes: &mut Attributes, key: CFStringRef, value: impl TCFType) {
    attributes.push((unsafe { CFString::wrap_under_get_rule(key) }, value.into_CFType()));
}
fn dictionary(attributes: &Attributes) -> CFDictionary<CFString, CFType> {
    CFDictionary::from_CFType_pairs(attributes)
}
fn copy(attributes: &Attributes) -> io::Result<Option<CFType>> {
    let query = dictionary(attributes);
    let mut result = ptr::null();
    let status = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &mut result) };
    match status {
        -25300 => Ok(None),
        0 if !result.is_null() => Ok(Some(unsafe { CFType::wrap_under_create_rule(result) })),
        0 => Err(io::Error::other("Keychain returned no requested value")),
        _ => Err(super::failure(security_framework::base::Error::from_code(status))),
    }
}
fn bytes(value: &CFType) -> io::Result<Vec<u8>> {
    let data =
        value.downcast::<CFData>().ok_or_else(|| io::Error::other("invalid Keychain data"))?;
    if data.len() > super::super::MAX_SECRET_BYTES as isize {
        return Err(io::Error::other("Keychain credential exceeds storage limit"));
    }
    Ok(data.bytes().to_vec())
}

fn read(service: &str, account: &str) -> io::Result<Option<Vec<u8>>> {
    let mut query = Policy::current()?.query(service, Some(account))?;
    unsafe {
        put(&mut query, kSecReturnData, CFBoolean::true_value());
    }
    let _timing = crate::trace::Span::new("credentials.keychain.read");
    copy(&query)?.as_ref().map(bytes).transpose()
}

#[cfg(not(test))]
pub(crate) fn read_confirmed(service: &str, account: &str) -> io::Result<Option<Vec<u8>>> {
    let mut query = Policy::current()?.query(service, Some(account))?;
    unsafe {
        put(&mut query, kSecAttrGeneric, CFData::from_buffer(&[1]));
        put(&mut query, kSecReturnData, CFBoolean::true_value());
    }
    copy(&query)?.as_ref().map(bytes).transpose()
}

pub(crate) fn save(
    service: &str,
    account: &str,
    email: Option<&str>,
    data: &[u8],
    confirmed: bool,
) -> io::Result<()> {
    if data.len() != super::super::MAX_SECRET_BYTES {
        return Err(io::Error::other("credential exceeds storage limit"));
    }
    let policy = Policy::current()?;
    let query = policy.query(service, Some(account))?;
    let mut updates = vec![];
    unsafe {
        put(&mut updates, kSecValueData, CFData::from_buffer(data));
        put(&mut updates, kSecAttrLabel, CFString::new(service));
        // Only confirmed account keys are offered for explicit recovery.
        put(&mut updates, kSecAttrGeneric, CFData::from_buffer(&[u8::from(confirmed)]));
        if let Some(email) = email {
            put(&mut updates, kSecAttrDescription, CFString::new(email));
        }
    }
    let mut attributes = query.clone();
    attributes.extend(updates.clone());
    policy.accessibility(&mut attributes);
    let _timing = crate::trace::Span::new("credentials.keychain.write");
    let status =
        unsafe { SecItemAdd(dictionary(&attributes).as_concrete_TypeRef(), ptr::null_mut()) };
    if status == 0 {
        return Ok(());
    }
    if status != -25299 {
        return Err(super::failure(security_framework::base::Error::from_code(status)));
    }
    if read(service, account)?.as_deref() != Some(data) {
        return Err(io::Error::other(
            "A different account key is already saved; recover that key before replacing it",
        ));
    }
    updates.retain(|(key, _)| unsafe { *key != CFString::wrap_under_get_rule(kSecValueData) });
    let status = unsafe {
        SecItemUpdate(
            dictionary(&query).as_concrete_TypeRef(),
            dictionary(&updates).as_concrete_TypeRef(),
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(super::failure(security_framework::base::Error::from_code(status)))
    }
}

#[cfg(any(test, feature = "connected-engine"))]
pub(crate) fn account(service: &str) -> io::Result<Option<super::super::AccountCredential>> {
    let mut query = Policy::current()?.query(service, None)?;
    unsafe {
        put(&mut query, kSecAttrGeneric, CFData::from_buffer(&[1]));
        put(&mut query, kSecReturnData, CFBoolean::true_value());
        put(&mut query, kSecReturnAttributes, CFBoolean::true_value());
        // Two results suffice to distinguish a unique account from an ambiguous choice.
        put(&mut query, kSecMatchLimit, CFNumber::from(2));
    }
    let _timing = crate::trace::Span::new("credentials.keychain.read");
    let Some(result) = copy(&query)? else {
        return Ok(None);
    };
    let values = result
        .downcast::<CFArray>()
        .ok_or_else(|| io::Error::other("invalid Keychain account results"))?;
    if values.len() != 1 {
        return Ok(None);
    }
    let item = unsafe { CFType::wrap_under_get_rule(*values.get(0).expect("one result")) };
    let item = item
        .downcast::<CFDictionary>()
        .ok_or_else(|| io::Error::other("invalid Keychain account attributes"))?;
    let item: CFDictionary<CFString, CFType> =
        unsafe { CFDictionary::wrap_under_get_rule(item.as_concrete_TypeRef()) };
    unsafe {
        let account = text_attribute(&item, kSecAttrAccount)?;
        let email = text_attribute(&item, kSecAttrDescription).unwrap_or_default();
        let data = item
            .find(CFString::wrap_under_get_rule(kSecValueData))
            .ok_or_else(|| io::Error::other("Keychain account key is missing"))?;
        super::super::AccountCredential::parse(&account, email, &bytes(&data)?).map(Some)
    }
}
#[cfg(any(test, feature = "connected-engine"))]
unsafe fn text_attribute(
    item: &CFDictionary<CFString, CFType>,
    key: CFStringRef,
) -> io::Result<String> {
    item.find(unsafe { CFString::wrap_under_get_rule(key) })
        .and_then(|value| value.downcast::<CFString>())
        .map(|value| value.to_string())
        .ok_or_else(|| io::Error::other("Keychain account metadata is missing"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioned_account_keys_are_synchronizable() {
        // Eligible distribution builds explicitly request synchronized recovery copies.
        for group in [None, Some("TEAM.playbit".into())] {
            let policy = Policy { group };
            if policy.group.is_none() {
                assert!(policy.query("playbit.account_key", Some("12345")).is_err());
                continue;
            }
            let mut attributes = policy.query("playbit.account_key", Some("12345")).expect("query");
            policy.accessibility(&mut attributes);
            if policy.group.is_some() {
                let query = dictionary(&attributes);
                unsafe {
                    assert_eq!(
                        query
                            .find(CFString::wrap_under_get_rule(kSecAttrSynchronizable))
                            .expect("sync flag")
                            .downcast::<CFBoolean>()
                            .expect("boolean"),
                        CFBoolean::from(policy.group.is_some())
                    );
                }
            }
        }
    }

    #[test]
    #[ignore = "writes and removes synthetic recovery credentials in the local Keychain"]
    fn actual_recovery_discovery_returns_metadata_and_refuses_ambiguous_accounts() {
        // The production SecItem query must return both attributes and raw key bytes.
        let mut random = [0; 16];
        getrandom::fill(&mut random).expect("random service");
        let service = format!("playbit.test.{}", crate::store::Id::from_bytes(random));
        let first = crate::store::Id::from_bytes([1; 16]).to_string();
        let second = crate::store::Id::from_bytes([2; 16]).to_string();
        let result = (|| -> io::Result<()> {
            save(&service, &first, None, &[7; 16], false)?;
            assert_eq!(read(&service, &first)?, Some(vec![7; 16]));
            assert!(account(&service)?.is_none(), "pending keys cannot enroll another device");
            save(&service, &first, Some(""), &[7; 16], true)?;
            assert!(account(&service)?.expect("confirmed without email").email.is_empty());
            save(&service, &first, Some("test@example.com"), &[7; 16], true)?;
            let found = account(&service)?.expect("unique credential");
            assert_eq!(found.account.to_string(), first);
            assert_eq!(found.secret, [7; 16]);
            assert_eq!(found.email, "test@example.com");
            let mut timings = Vec::with_capacity(100);
            for _ in 0..100 {
                let start = std::time::Instant::now();
                assert!(account(&service)?.is_some());
                timings.push(start.elapsed().as_micros());
            }
            timings.sort_unstable();
            crate::diagnostic!(
                "account discovery n=100 p50={}us p99={}us",
                timings[49],
                timings[98]
            );
            save(&service, &second, Some("other@example.com"), &[8; 16], true)?;
            assert!(account(&service)?.is_none());
            Ok(())
        })();
        for id in [&first, &second] {
            // Delete from the same backend/group used by the signed test host.
            let query =
                Policy::current().expect("policy").query(&service, Some(id)).expect("query");
            let status = unsafe { SecItemDelete(dictionary(&query).as_concrete_TypeRef()) };
            assert!(status == 0 || status == -25300, "synthetic credential cleanup: {status}");
        }
        result.expect("SecItem recovery round trip");
    }
}
