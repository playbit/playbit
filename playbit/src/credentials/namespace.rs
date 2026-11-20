//! Distribution recovery follows the host and channel. Explicit desktop test
//! profiles have separate namespaces and cannot adopt the user's recovery copy.
#[cfg(not(all(target_os = "macos", feature = "process")))]
use core_foundation::{bundle::CFBundle, string::CFString};
use std::{io, path::Path};

pub(crate) struct Namespace {
    prefix: String,
    suffix: String,
}

impl Namespace {
    /// The caller has created the profile directory before resolving its scope.
    pub(crate) fn for_profile(profile: &Path) -> io::Result<Self> {
        // Playbit's tools and engine share the profile's compiled identity.
        // Embedded native/iOS hosts supply their own application bundle identity.
        #[cfg(all(not(test), target_os = "macos", feature = "process"))]
        let app = crate::paths::PROFILE_NAME;
        #[cfg(all(not(test), not(all(target_os = "macos", feature = "process"))))]
        let identifier = application_identifier();
        #[cfg(all(not(test), not(all(target_os = "macos", feature = "process"))))]
        let app = identifier.as_deref().unwrap_or(crate::paths::PROFILE_NAME);
        #[cfg(test)]
        return Ok(Self {
            prefix: "playbit.test".into(),
            suffix: format!(
                ".{}",
                blake3::hash(profile.canonicalize()?.as_os_str().as_encoded_bytes()).to_hex()
            ),
        });
        #[cfg(not(test))]
        let channel = crate::engine::profile::for_profile(profile)
            .map_err(|error| io::Error::other(error.to_string()))?;
        #[cfg(not(test))]
        let suffix = match channel {
            crate::release_channel::ReleaseChannel::Public => String::new(),
            crate::release_channel::ReleaseChannel::Testing => ".testing".into(),
        };
        #[cfg(all(not(test), target_os = "macos"))]
        let suffix = {
            let mut suffix = suffix;
            let profile = profile.canonicalize()?;
            if !crate::engine::profile::is_default_profile(&profile, channel)
                .map_err(|error| io::Error::other(error.to_string()))?
            {
                let id = crate::identifier::encode(
                    blake3::hash(profile.as_os_str().as_encoded_bytes()).as_bytes(),
                );
                suffix.push_str(".profile.");
                suffix.push_str(&id);
            }
            suffix
        };
        #[cfg(not(test))]
        Ok(Self {
            prefix: if app == crate::paths::PROFILE_NAME {
                "playbit".into()
            } else {
                format!("{app}.playbit")
            },
            suffix,
        })
    }

    pub(crate) fn service(&self) -> String {
        format!("{}.account_key{}", self.prefix, self.suffix)
    }
}

#[cfg(not(all(target_os = "macos", feature = "process")))]
fn application_identifier() -> Option<String> {
    let bundle = CFBundle::main_bundle();
    let info = bundle.info_dictionary();
    info.find(CFString::new("CFBundleIdentifier"))
        .and_then(|value| value.downcast::<CFString>())
        .map(|value| value.to_string())
}
