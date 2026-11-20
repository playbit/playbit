//! Runtime environments for Playbit clients. Distribution and
//! download filenames never select account authority or a local profile.
use serde::{Deserialize, Serialize};

mod endpoints {
    include!(concat!(env!("OUT_DIR"), "/release_channel_endpoints.rs"));
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum ReleaseChannel {
    #[default]
    Public,
    Testing,
    #[cfg(test)]
    #[serde(rename = "test-a")]
    FixtureA,
    #[cfg(test)]
    #[serde(rename = "test-b")]
    FixtureB,
}

impl ReleaseChannel {
    #[cfg(not(test))]
    pub const ALL: [Self; 2] = [Self::Public, Self::Testing];
    #[cfg(test)]
    pub const ALL: [Self; 4] = [Self::Public, Self::Testing, Self::FixtureA, Self::FixtureB];

    pub const fn is_personal(self) -> bool {
        !matches!(self, Self::Public | Self::Testing)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Public => "Public",
            Self::Testing => "Testing",
            #[cfg(test)]
            Self::FixtureB => "Fixture B",
            #[cfg(test)]
            Self::FixtureA => "Fixture A",
        }
    }

    pub fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "public" => Ok(Self::Public),
            "testing" => Ok(Self::Testing),
            #[cfg(test)]
            "test-b" => Ok(Self::FixtureB),
            #[cfg(test)]
            "test-a" => Ok(Self::FixtureA),
            _ => Err("PLAYBIT_CHANNEL must be public or testing"),
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Testing => "testing",
            #[cfg(test)]
            Self::FixtureB => "test-b",
            #[cfg(test)]
            Self::FixtureA => "test-a",
        }
    }

    pub const fn ap_hostname(self) -> &'static str {
        match self {
            Self::Public => endpoints::PUBLIC_AP_HOSTNAME,
            Self::Testing => endpoints::TESTING_AP_HOSTNAME,
            #[cfg(test)]
            Self::FixtureB => "ap.fixture-b.example.invalid",
            #[cfg(test)]
            Self::FixtureA => "ap.fixture-a.example.invalid",
        }
    }

    pub const fn website(self) -> &'static str {
        match self {
            Self::Public => endpoints::PUBLIC_WEBSITE,
            Self::Testing => endpoints::TESTING_WEBSITE,
            #[cfg(test)]
            Self::FixtureB => "https://fixture-b.example.invalid",
            #[cfg(test)]
            Self::FixtureA => "https://fixture-a.example.invalid",
        }
    }

    pub const fn macos_manifest(self) -> Option<&'static str> {
        match self {
            Self::Public => Some(endpoints::PUBLIC_MACOS_MANIFEST),
            Self::Testing => Some(endpoints::TESTING_MACOS_MANIFEST),
            #[cfg(test)]
            Self::FixtureA | Self::FixtureB => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_is_explicit_and_endpoints_follow_the_environment() {
        // Invalid explicit values never select a different account authority.
        for channel in [ReleaseChannel::Public, ReleaseChannel::Testing] {
            assert_eq!(ReleaseChannel::parse(channel.name()), Ok(channel));
            assert!(channel.website().starts_with("https://"));
            assert!(
                channel
                    .macos_manifest()
                    .expect("distributed update feed")
                    .starts_with("https://playbit.app/download/")
            );
        }
        for invalid in ["", "Public", "internal", " testing", "testing\n"] {
            assert!(ReleaseChannel::parse(invalid).is_err());
        }
        assert_ne!(ReleaseChannel::Public.ap_hostname(), ReleaseChannel::Testing.ap_hostname());
    }
}
