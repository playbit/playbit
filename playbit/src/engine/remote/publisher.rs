//! Publisher trust is captured from the authenticated AP identity and signed
//! status replies on the existing bounded network lane. No package-supplied
//! issuer or arbitrary response field can establish trust.
use super::{Remote, Result};
use crate::{
    network::ap::{operation::ServiceRequest, publisher::issuer_key_from_certificate},
    store::{Id, LocalStore},
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Issued {
    certificate: Vec<u8>,
    status: Vec<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Status {
    status: Vec<u8>,
}

fn now() -> Result<u64> {
    Ok(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs())
}

impl Remote {
    /// Local retained admission stays offline. Only an unavailable first
    /// admission uses the authenticated AP's exact-build receipt.
    pub(crate) fn authenticate_public_package(
        &mut self,
        store: &LocalStore,
        package: crate::package::Package,
    ) -> Result<crate::package::Package> {
        if let Ok(package) = store.authenticate_package(package.clone(), now()?) {
            return Ok(package);
        }
        let certificate =
            package.publisher_certificate().ok_or("package has no publisher signature")?;
        self.refresh_publisher_status(
            store,
            certificate.claims.publisher,
            certificate.claims.signer,
        )?;
        if let Ok(package) = store.authenticate_package(package.clone(), now()?) {
            return Ok(package);
        }
        let expected = self.build_app_identity(store, &package.id()?)?;
        let package = store.authenticate_package(package, now()?)?;
        if package.identity()? != &expected {
            return Err("build receipt has a different app identity".into());
        }
        Ok(package)
    }

    pub fn issue_publisher_certificate(&mut self, store: &LocalStore) -> Result<()> {
        let (signer, proof) = store.publisher_signing_request()?;
        let request = ServiceRequest::IssuePublisherCertificate {
            operation_id: Id::generate()?.to_string(),
            signer,
            proof,
        };
        let issued: Issued = serde_json::from_value(self.service_data(store, &request)?)?;
        let issuer = issuer_key_from_certificate(&self.identity.ca_certificate)?;
        store.cache_publisher_certificate(issuer, &issued.certificate, &issued.status, now()?)?;
        Ok(())
    }

    pub fn refresh_publisher_status(
        &mut self,
        store: &LocalStore,
        publisher: Id,
        signer: [u8; 32],
    ) -> Result<()> {
        let request = ServiceRequest::GetPublisherKeyStatus {
            operation_id: Id::generate()?.to_string(),
            publisher,
            signer,
        };
        let status: Status = serde_json::from_value(self.service_data(store, &request)?)?;
        let binding = crate::network::ap::publisher::KeyStatus::from_bytes(&status.status)?;
        if binding.claims.publisher != publisher || binding.claims.signer != signer {
            return Err("publisher status differs from requested signing key".into());
        }
        let issuer = issuer_key_from_certificate(&self.identity.ca_certificate)?;
        store.cache_publisher_status(issuer, &status.status, now()?)?;
        Ok(())
    }
}
