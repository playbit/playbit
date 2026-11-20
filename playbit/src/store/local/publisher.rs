//! Profile-local signer material and authenticated, monotonic publisher trust.
//! No signing seed, certificate cache or status record enters account replication.
use super::*;
use crate::network::ap::publisher::{KeyStatus, certificate_request_message};
use crate::package::signature::{
    PackageAdmission, PackageSigner, PublisherCertificate, PublisherTrust, TrustedSigner,
};

const CERTIFICATE: &[u8] = b"publisher/certificate";
const ISSUER_PREFIX: &str = "publisher/issuer/";
const STATUS_PREFIX: &str = "publisher/status/";
const MAX_CACHED_SIGNERS: usize = 1024;
const MAX_ISSUERS: usize = 16;
const ADMISSION_PREFIX: &str = "publisher/admitted-build/";
const ADMISSION_COUNT: &[u8] = b"publisher/admitted-build-count";
const MAX_ADMITTED_BUILDS: u32 = 4096;

pub(super) fn status_key(publisher: Id, signer: &[u8; 32]) -> String {
    format!("{STATUS_PREFIX}{publisher}/{}", crate::identifier::encode(signer))
}

impl LocalStore {
    fn publisher_seed(&self) -> Result<zeroize::Zeroizing<[u8; 32]>> {
        self.require_account_authority()?;
        let device = zeroize::Zeroizing::new(profile::seed(&self.metadata)?);
        Ok(zeroize::Zeroizing::new(blake3::derive_key(
            "app.playbit.publisher-signing-key.v1",
            device.as_ref(),
        )))
    }

    pub fn publisher_signing_request(&self) -> Result<([u8; 32], Vec<u8>)> {
        use ed25519_dalek::Signer;
        let pair = ed25519_dalek::SigningKey::from_bytes(&*self.publisher_seed()?);
        let signer = pair.verifying_key().to_bytes();
        let proof = pair.sign(&certificate_request_message(self.account.id, &signer));
        Ok((signer, proof.to_bytes().to_vec()))
    }

    /// `issuer` comes from the enrolled AP identity authenticated by the host,
    /// never from the candidate certificate or an arbitrary package's own key.
    pub fn cache_publisher_certificate(
        &self,
        issuer: [u8; 32],
        certificate: &[u8],
        status: &[u8],
        now: u64,
    ) -> Result<()> {
        let certificate = PublisherCertificate::from_bytes(certificate)
            .map_err(|error| Error::System(error.to_string()))?;
        let status = KeyStatus::from_bytes(status).map_err(Error::InvalidInput)?;
        let (signer, _) = self.publisher_signing_request()?;
        if certificate.claims.publisher != self.account.id || certificate.claims.signer != signer {
            return Err(Error::AccessDenied("publisher certificate belongs to another signer"));
        }
        verify_status(&status, &[issuer], now)?;
        let trusted = Self::trusted_signer(&status);
        certificate
            .verify(&PublisherTrust { issuers: &[issuer], signers: &[trusted] }, now)
            .map_err(|error| Error::System(error.to_string()))?;
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        self.save_publisher_status(&mut tx, issuer, &status)?;
        tx.put(
            CERTIFICATE,
            &certificate.to_bytes().map_err(|error| Error::System(error.to_string()))?,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn cache_publisher_status(&self, issuer: [u8; 32], bytes: &[u8], now: u64) -> Result<()> {
        let status = KeyStatus::from_bytes(bytes).map_err(Error::InvalidInput)?;
        verify_status(&status, &[issuer], now)?;
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        self.save_publisher_status(&mut tx, issuer, &status)?;
        tx.commit()?;
        Ok(())
    }

    fn save_publisher_status(
        &self,
        tx: &mut kvdb::WriteTxn<'_, RedbStorage>,
        issuer: [u8; 32],
        status: &KeyStatus,
    ) -> Result<()> {
        let key = status_key(status.claims.publisher, &status.claims.signer);
        if let Some(bytes) = tx.get(key.as_bytes())? {
            let previous = KeyStatus::from_bytes(&bytes).map_err(Error::InvalidInput)?;
            status.admit_update(&previous).map_err(Error::InvalidInput)?;
        } else if self.publisher_statuses()?.len() >= MAX_CACHED_SIGNERS {
            return Err(Error::LimitExceeded);
        }
        let issuer_key = format!("{ISSUER_PREFIX}{}", crate::identifier::encode(&issuer));
        if !tx.has(issuer_key.as_bytes())? && self.publisher_issuers()?.len() >= MAX_ISSUERS {
            return Err(Error::LimitExceeded);
        }
        tx.put(issuer_key.as_bytes(), &issuer)?;
        tx.put(key.as_bytes(), &status.to_bytes())?;
        Ok(())
    }

    pub(crate) fn publisher_issuers(&self) -> Result<Vec<[u8; 32]>> {
        let snapshot = self.metadata.begin_read()?;
        let mut issuers = Vec::new();
        for entry in snapshot.iter_from(ISSUER_PREFIX.as_bytes()) {
            let (key, bytes) = entry?;
            if !key.starts_with(ISSUER_PREFIX.as_bytes()) {
                break;
            }
            if issuers.len() == MAX_ISSUERS {
                return Err(Error::LimitExceeded);
            }
            issuers.push(
                bytes.as_ref().try_into().map_err(|_| Error::corrupt("publisher issuer width"))?,
            );
        }
        Ok(issuers)
    }

    /// Public evidence for distributing an authenticated package to an offline host.
    /// A key in the package alone cannot establish the host's issuer trust.
    #[cfg(any(feature = "app-host", test))]
    pub(crate) fn package_publisher_status(
        &self,
        package: &crate::package::Package,
        now: u64,
    ) -> crate::package::Result<KeyStatus> {
        let claims =
            &package.publisher_certificate().ok_or("missing publisher certificate")?.claims;
        let key = status_key(claims.publisher, &claims.signer);
        let snapshot = self.metadata.begin_read()?;
        let bytes = snapshot.get(key.as_bytes())?.ok_or("missing publisher-key status")?;
        let status = KeyStatus::from_bytes(&bytes)?;
        verify_status(&status, &self.publisher_issuers()?, now)?;
        if status.claims.issuer != claims.issuer
            || status.claims.publisher != package.identity()?.publisher
            || status.claims.signer != claims.signer
            || status.claims.revoked
            || status.claims.epoch < claims.revocation_epoch
        {
            return Err("publisher-key status differs from the authenticated package".into());
        }
        Ok(status)
    }

    fn publisher_statuses(&self) -> Result<Vec<KeyStatus>> {
        let snapshot = self.metadata.begin_read()?;
        let mut statuses = Vec::new();
        for entry in snapshot.iter_from(STATUS_PREFIX.as_bytes()) {
            let (key, bytes) = entry?;
            if !key.starts_with(STATUS_PREFIX.as_bytes()) {
                break;
            }
            if statuses.len() == MAX_CACHED_SIGNERS {
                return Err(Error::LimitExceeded);
            }
            let status = KeyStatus::from_bytes(&bytes).map_err(Error::InvalidInput)?;
            if key.as_ref() != status_key(status.claims.publisher, &status.claims.signer).as_bytes()
            {
                return Err(Error::corrupt("publisher status storage binding"));
            }
            statuses.push(status);
        }
        Ok(statuses)
    }

    fn trusted_signer(status: &KeyStatus) -> TrustedSigner {
        TrustedSigner {
            issuer: status.claims.issuer,
            publisher: status.claims.publisher,
            signer: status.claims.signer,
            epoch: status.claims.epoch,
            revoked: status.claims.revoked,
        }
    }

    fn publisher_trust(&self, now: u64) -> Result<(Vec<[u8; 32]>, Vec<TrustedSigner>)> {
        self.require_account_authority()?;
        let issuers = self.publisher_issuers()?;
        let statuses = self.publisher_statuses()?;
        let mut signers = Vec::with_capacity(statuses.len());
        for status in statuses {
            verify_status(&status, &issuers, now)?;
            signers.push(Self::trusted_signer(&status));
        }
        Ok((issuers, signers))
    }

    pub fn publisher_signer(&self, now: u64) -> crate::package::Result<PackageSigner> {
        let snapshot = self.metadata.begin_read()?;
        let bytes = snapshot.get(CERTIFICATE)?.ok_or(
            "This device has no publisher certificate. Connect and sign in before building an app.",
        )?;
        let certificate = PublisherCertificate::from_bytes(&bytes)?;
        let (issuers, signers) = self.publisher_trust(now)?;
        certificate.verify(&PublisherTrust { issuers: &issuers, signers: &signers }, now)?;
        if certificate.claims.publisher != self.account.id {
            return Err("publisher certificate belongs to another account".into());
        }
        PackageSigner::from_seed(certificate, &*self.publisher_seed()?)
    }

    pub fn authenticate_package(
        &self,
        package: crate::package::Package,
        now: u64,
    ) -> crate::package::Result<crate::package::Package> {
        let (issuers, signers) = self.publisher_trust(now)?;
        let trust = PublisherTrust { issuers: &issuers, signers: &signers };
        let key = format!("{ADMISSION_PREFIX}{}", package.id()?);
        let saved = {
            let snapshot = self.metadata.begin_read()?;
            snapshot
                .get(key.as_bytes())?
                .map(|bytes| PackageAdmission::from_trusted_bytes(&bytes))
                .transpose()?
        };
        if let Some(admission) = saved {
            return package.authenticate_admitted(&trust, &admission, now);
        }
        let (package, admission) = package.authenticate_first(&trust, now)?;
        self.save_package_admission(&admission)?;
        Ok(package)
    }

    /// The receipt authenticates only this exact build's earlier admission.
    /// Execution still verifies complete package bytes and current key status.
    pub fn cache_package_admission(&self, bytes: &[u8], build: [u8; 32], now: u64) -> Result<()> {
        let receipt = crate::network::ap::publisher::BuildAdmissionReceipt::from_bytes(bytes)
            .map_err(|error| Error::System(error.to_string()))?;
        let admission = receipt
            .verify(&self.publisher_issuers()?, &build, now)
            .map_err(|error| Error::System(error.to_string()))?;
        self.save_package_admission(&admission)
    }

    fn save_package_admission(&self, admission: &PackageAdmission) -> Result<()> {
        let key = format!("{ADMISSION_PREFIX}{}", crate::identifier::encode(&admission.build));
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        if let Some(previous) = tx.get(key.as_bytes())? {
            let previous = PackageAdmission::from_trusted_bytes(&previous)
                .map_err(|error| Error::System(error.to_string()))?;
            if previous.build != admission.build
                || previous.issuer != admission.issuer
                || previous.publisher != admission.publisher
                || previous.signer != admission.signer
            {
                return Err(Error::corrupt("retained build admission changed identity"));
            }
            return Ok(());
        }
        let count = tx
            .get(ADMISSION_COUNT)?
            .map(|bytes| {
                bytes
                    .as_ref()
                    .try_into()
                    .map(u32::from_le_bytes)
                    .map_err(|_| Error::corrupt("retained build admission count width"))
            })
            .transpose()?
            .unwrap_or(0);
        if count >= MAX_ADMITTED_BUILDS {
            return Err(Error::LimitExceeded);
        }
        tx.put(key.as_bytes(), &admission.to_bytes())?;
        tx.put(ADMISSION_COUNT, &(count + 1).to_le_bytes())?;
        tx.commit()?;
        Ok(())
    }
}

/// A real profile signer under an explicit synthetic issuer, for local Store tests.
#[cfg(test)]
pub(super) fn install_test_package(
    store: &LocalStore,
    package: crate::package::Package,
) -> crate::package::Result<crate::package::Package> {
    let package = crate::package::signature::testing::signed(store, package)?;
    let builds = store.path.join(crate::paths::BUILDS);
    fs::create_dir_all(&builds)?;
    fs::write(builds.join(package.id()?), package.to_bytes()?)?;
    store.remember_imported_app(&package)?;
    Ok(package)
}

fn verify_status(status: &KeyStatus, issuers: &[[u8; 32]], now: u64) -> Result<()> {
    status
        .verify_with(issuers, now, |key, message, signature| {
            crate::package::signature::verify_ed25519(key, message, signature).is_ok()
        })
        .map(|_| ())
        .map_err(Error::InvalidInput)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful isolated cryptographic fixtures.")]
mod admission_tests {
    use super::*;
    use crate::{
        network::ap::publisher::{BuildAdmissionReceipt, KeyStatusClaims},
        package::Package,
    };
    use ed25519_dalek::{SigningKey, pkcs8::EncodePrivateKey};

    fn signed(store: &LocalStore, asset: &[u8]) -> (Package, [u8; 32], Vec<u8>) {
        let issuer = SigningKey::from_bytes(&[7; 32]);
        let issuer_der = issuer.to_pkcs8_der().unwrap();
        let issuer_public = issuer.verifying_key().to_bytes();
        let (signer, _) = store.publisher_signing_request().unwrap();
        let certificate = PublisherCertificate::issue(
            crate::package::signature::CertificateClaims {
                issuer: issuer_public,
                publisher: store.account.id,
                signer,
                issued_at: 10,
                expires_at: 20,
                revocation_epoch: 1,
            },
            issuer_der.as_bytes(),
        )
        .unwrap();
        let status = KeyStatus::issue(
            KeyStatusClaims {
                issuer: issuer_public,
                publisher: store.account.id,
                signer,
                epoch: 1,
                checked_at: 10,
                revoked: false,
            },
            issuer_der.as_bytes(),
        )
        .unwrap();
        store
            .cache_publisher_certificate(
                issuer_public,
                &certificate.to_bytes().unwrap(),
                &status.to_bytes(),
                15,
            )
            .unwrap();
        let package = Package::new(
            "retained".into(),
            "Retained".into(),
            b"\0asm\x01\0\0\0".to_vec(),
            std::collections::BTreeMap::from([("asset".into(), asset.to_vec())]),
        )
        .unwrap();
        let signer = store.publisher_signer(15).unwrap();
        let mut bytes = Vec::new();
        package.write_signed(&signer, 15, &mut bytes).unwrap();
        (Package::from_bytes(&bytes).unwrap(), issuer_public, status.to_bytes().to_vec())
    }

    #[test]
    fn retained_exact_package_survives_expiry_and_reopen_but_not_known_revocation() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let (package, issuer, status) = signed(&store, b"first");
        let (unknown, _, _) = signed(&store, b"different exact build");
        let authenticated = store.authenticate_package(package.clone(), 15).unwrap();
        let portable = store.package_publisher_status(&authenticated, 15).unwrap();
        assert_eq!(portable.to_bytes().as_slice(), status);
        assert_eq!(portable.claims.issuer, issuer);
        assert!(store.authenticate_package(package.clone(), 14).is_err());
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        store.authenticate_package(package.clone(), 21).unwrap();
        assert!(store.authenticate_package(unknown, 21).is_err());
        assert!(store.publisher_signer(21).is_err());
        let claims = &package.publisher_certificate().unwrap().claims;
        let revoked = KeyStatus::issue(
            KeyStatusClaims {
                issuer,
                publisher: claims.publisher,
                signer: claims.signer,
                epoch: 2,
                checked_at: 22,
                revoked: true,
            },
            SigningKey::from_bytes(&[7; 32]).to_pkcs8_der().unwrap().as_bytes(),
        )
        .unwrap();
        store.cache_publisher_status(issuer, &revoked.to_bytes(), 22).unwrap();
        assert!(store.package_publisher_status(&authenticated, 22).is_err());
        assert!(store.authenticate_package(package, 22).is_err());
    }

    #[test]
    fn new_profile_needs_pinned_exact_ap_admission_for_expired_public_package() {
        let source = tempfile::tempdir().unwrap();
        let source = LocalStore::open(source.path()).unwrap();
        let (package, issuer, status) = signed(&source, b"public copy");
        let claims = &package.publisher_certificate().unwrap().claims;
        let receipt = BuildAdmissionReceipt::issue(
            SigningKey::from_bytes(&[7; 32]).to_pkcs8_der().unwrap().as_bytes(),
            package.digest(),
            claims.publisher,
            claims.signer,
            15,
        )
        .unwrap();
        let target = tempfile::tempdir().unwrap();
        let target = LocalStore::open(target.path()).unwrap();
        assert!(target.cache_package_admission(&receipt.to_bytes(), package.digest(), 21).is_err());
        target.cache_publisher_status(issuer, &status, 21).unwrap();
        assert!(target.authenticate_package(package.clone(), 21).is_err());
        assert!(target.cache_package_admission(&receipt.to_bytes(), [9; 32], 21).is_err());
        assert!(target.cache_package_admission(&receipt.to_bytes(), package.digest(), 14).is_err());
        target.cache_package_admission(&receipt.to_bytes(), package.digest(), 21).unwrap();
        target.authenticate_package(package, 21).unwrap();
    }
}
