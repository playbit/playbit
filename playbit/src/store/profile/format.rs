//! Profile-owned logical requirements, separate from redb/kvdb physical formats.
use super::*;

pub const KEY: &[u8] = b"logical-format";
pub const MAX_BYTES: usize = 4096;
pub const BASELINE: &[u8] = br#"{"format":"playbit.profile","revision":1,"required_read_features":[],"required_write_features":[]}"#;
pub const LOCAL_APP_SCOPES: &str = "local-app-scopes.v1";
pub const HOSTED_APP_SCOPES: &str = "hosted-app-scopes.v1";
pub const SHARED_SESSION: &str = "shared-session.v1";
pub const UPDATE_REQUIRED: &str = "This profile requires a different version of Playbit. Its saved data has been kept. Open it with a compatible version or choose a new empty profile.";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    format: String,
    revision: u32,
    required_read_features: Vec<String>,
    required_write_features: Vec<String>,
}
pub fn unsupported() -> Error {
    Error::InvalidInput(UPDATE_REQUIRED)
}
fn parse(bytes: &[u8]) -> Result<Descriptor> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return Err(Error::corrupt("profile format descriptor exceeds its bound"));
    }
    let value: Descriptor = serde_json::from_slice(bytes)
        .map_err(|_| Error::corrupt("invalid profile format descriptor"))?;
    if value.format != "playbit.profile" || value.revision != 1 {
        return Err(unsupported());
    }
    for names in [&value.required_read_features, &value.required_write_features] {
        if names.len() > 16
            || names.iter().any(|name| {
                name.is_empty()
                    || name.len() > 64
                    || !name.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
            })
            || names.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(Error::corrupt("invalid profile format requirements"));
        }
    }
    Ok(value)
}
fn validate_supported(bytes: &[u8], supported: &[&str]) -> Result<()> {
    let value = parse(bytes)?;
    for name in value.required_read_features.iter().chain(&value.required_write_features) {
        if !supported.contains(&name.as_str()) {
            return Err(unsupported());
        }
    }
    Ok(())
}
pub fn validate(bytes: &[u8]) -> Result<()> {
    validate_supported(bytes, &[LOCAL_APP_SCOPES, HOSTED_APP_SCOPES, SHARED_SESSION])
}
/// Add the minimum capability needed to keep older clients from treating local
/// app data as an ordinary replicable volume. Unknown requirements never vanish.
#[cfg(test)]
pub fn promote_local_app_scopes(bytes: &[u8]) -> Result<Vec<u8>> {
    promote_app_scopes(bytes, LOCAL_APP_SCOPES)
}

#[cfg(any(test, all(not(target_arch = "wasm32"), feature = "local-store")))]
pub(crate) fn promote_app_scopes(bytes: &[u8], feature: &str) -> Result<Vec<u8>> {
    validate(bytes)?;
    let mut value = parse(bytes)?;
    let mut changed = false;
    for names in [&mut value.required_read_features, &mut value.required_write_features] {
        if let Err(index) = names.binary_search_by(|name| name.as_str().cmp(feature)) {
            if names.len() == 16 {
                return Err(Error::corrupt("profile format requirements exceed their bound"));
            }
            names.insert(index, feature.into());
            changed = true;
        }
    }
    if !changed {
        return Ok(bytes.to_vec());
    }
    let promoted = serde_json::to_vec(&value)
        .map_err(|_| Error::corrupt("cannot encode profile format requirements"))?;
    validate(&promoted)?;
    Ok(promoted)
}
pub fn read(tx: &ReadTxn<'_, impl Storage>) -> Result<()> {
    let value = tx.value_reader(KEY)?.ok_or_else(unsupported)?;
    if value.len() > MAX_BYTES {
        return Err(Error::corrupt("profile format descriptor exceeds its bound"));
    }
    let mut bytes = [0; MAX_BYTES];
    value.read_exact_at(0, &mut bytes[..value.len()])?;
    validate(&bytes[..value.len()])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_app_scopes_are_explicit_and_older_readers_refuse_them() {
        let promoted = promote_local_app_scopes(BASELINE).expect("baseline promotion is supported");
        let value = parse(&promoted).expect("promoted descriptor is valid");
        assert_eq!(value.required_read_features, [LOCAL_APP_SCOPES]);
        assert_eq!(value.required_write_features, [LOCAL_APP_SCOPES]);
        assert!(validate(&promoted).is_ok());
        assert!(validate_supported(&promoted, &[]).is_err());
        assert!(validate_supported(BASELINE, &[]).is_ok());
        assert_eq!(
            promote_local_app_scopes(&promoted).expect("repeated promotion is supported"),
            promoted
        );
        let hosted = promote_app_scopes(&promoted, HOSTED_APP_SCOPES)
            .expect("hosted scope promotion is supported");
        assert!(validate(&hosted).is_ok());
        assert!(validate_supported(&hosted, &[LOCAL_APP_SCOPES]).is_err());
        let unknown = br#"{"format":"playbit.profile","revision":1,"required_read_features":["future.authority"],"required_write_features":[]}"#;
        assert!(promote_local_app_scopes(unknown).is_err());
        let duplicate = br#"{"format":"playbit.profile","revision":1,"required_read_features":["local-app-scopes.v1","local-app-scopes.v1"],"required_write_features":[]}"#;
        assert!(promote_local_app_scopes(duplicate).is_err());
    }
    #[test]
    fn shared_sessions_require_explicit_read_and_write_support() {
        let local = promote_local_app_scopes(BASELINE).expect("local scopes are supported");
        let shared = promote_app_scopes(&local, SHARED_SESSION).expect("Shared is supported");
        let descriptor = parse(&shared).expect("promoted descriptor is valid");
        assert_eq!(descriptor.required_read_features, [LOCAL_APP_SCOPES, SHARED_SESSION]);
        assert_eq!(descriptor.required_write_features, [LOCAL_APP_SCOPES, SHARED_SESSION]);
        assert!(validate_supported(&shared, &[LOCAL_APP_SCOPES]).is_err());
        assert!(validate(&shared).is_ok());
        assert_eq!(
            promote_app_scopes(&shared, SHARED_SESSION).expect("promotion is idempotent"),
            shared
        );
        assert!(validate_supported(&local, &[LOCAL_APP_SCOPES]).is_ok());
    }
    #[test]
    fn bounded_profile_requirements_refuse_before_body_interpretation() {
        assert!(validate(BASELINE).is_ok());
        for bytes in [
            br#"{"format":"other","revision":1,"required_read_features":[],"required_write_features":[]}"#.as_slice(),
            br#"{"format":"playbit.profile","revision":2,"required_read_features":[],"required_write_features":[]}"#,
            br#"{"format":"playbit.profile","revision":1,"required_read_features":[],"required_write_features":["future.authority"]}"#,
            br#"{"format":"playbit.profile","revision":1,"required_read_features":["future.authority"],"required_write_features":[]}"#,
            br#"{"format":"playbit.profile","revision":1,"revision":1,"required_read_features":[],"required_write_features":[]}"#,
            b"{}",
        ] {
            assert!(validate(bytes).is_err());
        }
    }
}
