//! Explicit synthetic publisher authority for isolated tests, never production trust.
use super::*;
use crate::package::Package;
use ed25519_dalek::pkcs8::EncodePrivateKey;

pub(crate) fn certificate(
    publisher: Id,
    signer: [u8; 32],
    now: u64,
) -> Result<(PublisherCertificate, TrustedSigner)> {
    let issuer = SigningKey::from_bytes(&[7; 32]);
    let claims = CertificateClaims {
        issuer: issuer.verifying_key().to_bytes(),
        publisher,
        signer,
        issued_at: now.saturating_sub(1).max(1),
        expires_at: now
            .checked_add(MAX_CERTIFICATE_LIFETIME_SECS - 1)
            .ok_or("fixture time overflow")?,
        revocation_epoch: 1,
    };
    let status =
        TrustedSigner { issuer: claims.issuer, publisher, signer, epoch: 1, revoked: false };
    Ok((PublisherCertificate::issue(claims, issuer.to_pkcs8_der()?.as_bytes())?, status))
}

#[cfg(test)]
pub(crate) fn authenticate(package: &Package, publisher: Id, now: u64) -> Result<Package> {
    let key = SigningKey::from_bytes(&[8; 32]);
    let (certificate, status) = certificate(publisher, key.verifying_key().to_bytes(), now)?;
    let issuer = certificate.claims.issuer;
    let signer = PackageSigner::from_seed(certificate, &[8; 32])?;
    let mut bytes = Vec::new();
    package.write_signed(&signer, now, &mut bytes)?;
    Package::from_bytes(&bytes)?
        .authenticate(&PublisherTrust { issuers: &[issuer], signers: &[status] }, now)
}

/// Enroll this isolated profile's real signing key under an explicit fixture pin.
/// Qualification may use this only while creating its fresh synthetic profile.
#[cfg(all(
    not(target_arch = "wasm32"),
    feature = "local-store",
    any(test, feature = "ap-identity")
))]
pub fn signed(store: &crate::store::LocalStore, package: Package) -> Result<Package> {
    use crate::network::ap::publisher::{KeyStatus, KeyStatusClaims};
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let (signer_key, _) = store.publisher_signing_request()?;
    let (certificate, _) = certificate(store.account().id, signer_key, now)?;
    let issuer_key = certificate.claims.issuer;
    let issuer = SigningKey::from_bytes(&[7; 32]).to_pkcs8_der()?;
    let status = KeyStatus::issue(
        KeyStatusClaims {
            issuer: issuer_key,
            publisher: store.account().id,
            signer: signer_key,
            epoch: 1,
            checked_at: now,
            revoked: false,
        },
        issuer.as_bytes(),
    )?;
    store.cache_publisher_certificate(
        issuer_key,
        &certificate.to_bytes()?,
        &status.to_bytes(),
        now,
    )?;
    let signed_at = certificate.claims.issued_at;
    let signer = store.publisher_signer(now)?;
    let mut bytes = Vec::new();
    package.write_signed(&signer, signed_at, &mut bytes)?;
    store.authenticate_package(Package::from_bytes(&bytes)?, now)
}
