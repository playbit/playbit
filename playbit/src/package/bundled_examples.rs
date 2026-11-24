//! The Mac release carries two previously admitted public packages. Its embedded
//! issuer is scoped to these exact build digests, including in a fresh offline profile.
use crate::{
    network::ap::publisher::BuildAdmissionReceipt,
    package::{Package, Result},
};
use crate::{
    network::ap::publisher::KeyStatus,
    package::signature::{PublisherTrust, TrustedSigner},
};

use crate::release_channel::ReleaseChannel;

pub(crate) struct Evidence {
    channel: ReleaseChannel,
    app_id: &'static str,
    publisher: &'static str,
    issuer: &'static [u8; 32],
    status: &'static [u8],
    receipt: &'static [u8],
}

impl Evidence {
    pub(crate) fn for_build(build: &str) -> Option<Self> {
        let (channel, app_id, receipt): (_, _, &[u8]) = match build {
            "DJoagkAkSDrTM4oj3YJEuQREDydG4sGE4kbpeWeZx6BB" => (
                ReleaseChannel::Public,
                "football",
                include_bytes!("../../platform/macos/examples/football.admission"),
            ),
            "65E3jmSAUhUdkmrYrTeRrh32pWiwqkDz2FKu9wB84yfx" => (
                ReleaseChannel::Public,
                "little-list",
                include_bytes!("../../platform/macos/examples/little-list.admission"),
            ),
            "HfGyXEJgBcfzT6d4S4wS35ZNCaiUypjP4vTfYWaQKsN6" => (
                ReleaseChannel::Testing,
                "football",
                include_bytes!("../../platform/macos/examples/football-testing.admission"),
            ),
            "HwkgRPS7UTsBbwnnw9dCVhx6YGuwjeThqFH59uYNcqrQ" => (
                ReleaseChannel::Testing,
                "little-list",
                include_bytes!("../../platform/macos/examples/little-list-testing.admission"),
            ),
            _ => return None,
        };
        let (publisher, issuer, status): (_, &[u8; 32], &[u8]) =
            if channel == ReleaseChannel::Public {
                (
                    "BQuhAXnZ9znNKiaapEyVNo",
                    include_bytes!("../../platform/macos/examples/issuer.public"),
                    include_bytes!("../../platform/macos/examples/signer.status"),
                )
            } else {
                (
                    "R8UTnVVXKZeS5FLfoLmwVt",
                    include_bytes!("../../platform/macos/examples/testing-issuer.public"),
                    include_bytes!("../../platform/macos/examples/testing-signer.status"),
                )
            };
        Some(Self { channel, app_id, publisher, issuer, status, receipt })
    }
    pub(crate) fn for_profile(profile: &std::path::Path, build: &str) -> Result<Option<Self>> {
        let Some(known) = Self::for_build(build) else { return Ok(None) };
        Ok((known.channel == crate::engine::profile::for_profile(profile)?).then_some(known))
    }
    /// The release's exact build, app and realm must agree before naming its publisher.
    pub(crate) fn identifies(
        &self,
        channel: ReleaseChannel,
        app: &crate::package::AppIdentity,
    ) -> bool {
        self.channel == channel
            && self.app_id == app.app_id
            && self.publisher == app.publisher.to_string()
    }
    pub(crate) fn status(&self, cached: Option<KeyStatus>, now: u64) -> Result<KeyStatus> {
        let shipped = KeyStatus::from_bytes(self.status)?;
        shipped.verify(&[*self.issuer], now)?;
        if let Some(cached) = cached.filter(|status| status.claims.issuer == *self.issuer) {
            cached.verify(&[*self.issuer], now)?;
            if cached.claims.publisher != shipped.claims.publisher
                || cached.claims.signer != shipped.claims.signer
            {
                return Err("bundled publisher status binding changed".into());
            }
            if cached.claims.revoked {
                return Err("bundled publisher signer is revoked".into());
            }
            if cached.claims.checked_at >= shipped.claims.checked_at {
                cached.admit_update(&shipped)?;
                return Ok(cached);
            }
        }
        Ok(shipped)
    }

    pub(crate) fn authenticate(
        &self,
        package: Package,
        signer: TrustedSigner,
        now: u64,
    ) -> Result<Package> {
        let admission = BuildAdmissionReceipt::from_bytes(self.receipt)?.verify(
            &[*self.issuer],
            &package.digest(),
            now,
        )?;
        package.authenticate_admitted(
            &PublisherTrust { issuers: &[*self.issuer], signers: &[signer] },
            &admission,
            now,
        )
    }
}
