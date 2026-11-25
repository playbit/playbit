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
    Rasmus,
    Edward,
    Julia,
}

impl ReleaseChannel {
    pub const ALL: [Self; 5] =
        [Self::Public, Self::Testing, Self::Rasmus, Self::Edward, Self::Julia];

    pub const fn is_personal(self) -> bool {
        matches!(self, Self::Rasmus | Self::Edward | Self::Julia)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Public => "Public",
            Self::Testing => "Testing",
            Self::Rasmus => "Rasmus Testing",
            Self::Edward => "Edward Testing",
            Self::Julia => "Julia Testing",
        }
    }

    pub fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "public" => Ok(Self::Public),
            "testing" => Ok(Self::Testing),
            "rasmus" => Ok(Self::Rasmus),
            "edward" => Ok(Self::Edward),
            "julia" => Ok(Self::Julia),
            _ => Err("PLAYBIT_CHANNEL must be public, testing, rasmus, edward or julia"),
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Testing => "testing",
            Self::Rasmus => "rasmus",
            Self::Edward => "edward",
            Self::Julia => "julia",
        }
    }

    pub const fn ap_hostname(self) -> &'static str {
        match self {
            Self::Public => endpoints::PUBLIC_AP_HOSTNAME,
            Self::Testing => endpoints::TESTING_AP_HOSTNAME,
            Self::Rasmus => "ap.rasmus.testing.playbit.app",
            Self::Edward => "ap.edward.testing.playbit.app",
            Self::Julia => "ap.julia.testing.playbit.app",
        }
    }

    pub const fn website(self) -> &'static str {
        match self {
            Self::Public => endpoints::PUBLIC_WEBSITE,
            Self::Testing => endpoints::TESTING_WEBSITE,
            Self::Rasmus => "https://rasmus.testing.playbit.app",
            Self::Edward => "https://edward.testing.playbit.app",
            Self::Julia => "https://julia.testing.playbit.app",
        }
    }

    pub const fn macos_manifest(self) -> Option<&'static str> {
        match self {
            Self::Public => Some(endpoints::PUBLIC_MACOS_MANIFEST),
            Self::Testing => Some(endpoints::TESTING_MACOS_MANIFEST),
            Self::Rasmus | Self::Edward | Self::Julia => None,
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
