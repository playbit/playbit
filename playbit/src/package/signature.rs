//! Offline package authorship. Embedded keys are claims until a host admits the
//! issuer and its monotonically cached signer status. Reader keys grant no signing authority.
use super::{AppIdentity, Result};
use crate::store::Id;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey, pkcs8::DecodePrivateKey};

#[cfg(any(target_arch = "wasm32", feature = "ap-protocol"))]
pub mod admission;
pub mod status;

pub const CERTIFICATE_BYTES: usize = 169;
pub const SIGNATURE_BYTES: usize = 249;
pub const MAX_CERTIFICATE_LIFETIME_SECS: u64 = 365 * 24 * 60 * 60;
const CLAIM_BYTES: usize = 105;
const CERTIFICATE_DOMAIN: &[u8] = b"app.playbit.publisher-certificate.v1\0";
const PACKAGE_DOMAIN: &[u8] = b"app.playbit.package-signature.v1\0";
pub(crate) const CONTENT_DOMAIN: &[u8] = b"app.playbit.signed-package-content.v1\0";
const MAGIC: &[u8; 8] = b"PBSIG001";
const _: () = assert!(CERTIFICATE_BYTES == CLAIM_BYTES + 64);
const _: () = assert!(SIGNATURE_BYTES == MAGIC.len() + CERTIFICATE_BYTES + 8 + 64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificateClaims {
    pub issuer: [u8; 32],
    pub publisher: Id,
    pub signer: [u8; 32],
    pub issued_at: u64,
    pub expires_at: u64,
    pub revocation_epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublisherCertificate {
    pub claims: CertificateClaims,
    pub signature: [u8; 64],
}

/// Host-admitted state from a verified status snapshot, never from a package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedSigner {
    pub issuer: [u8; 32],
    pub publisher: Id,
    pub signer: [u8; 32],
    pub epoch: u64,
    pub revoked: bool,
}

pub struct PublisherTrust<'a> {
    pub issuers: &'a [[u8; 32]],
    pub signers: &'a [TrustedSigner],
}

/// Evidence admitted by the host, from protected local metadata or a verified
/// AP receipt. A package's own signing timestamp cannot create this evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageAdmission {
    pub(crate) issuer: [u8; 32],
    pub(crate) build: [u8; 32],
    pub(crate) publisher: Id,
    pub(crate) signer: [u8; 32],
    pub(crate) admitted_at: u64,
}

#[cfg(any(target_arch = "wasm32", feature = "ap-protocol"))]
impl PackageAdmission {
    pub(crate) const BYTES: usize = 121;

    pub(crate) fn from_trusted_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != Self::BYTES || bytes[0] != 1 {
            return Err("unsupported retained package admission".into());
        }
        Ok(Self {
            issuer: bytes[1..33].try_into()?,
            build: bytes[33..65].try_into()?,
            publisher: Id::from_bytes(bytes[65..81].try_into()?),
            signer: bytes[81..113].try_into()?,
            admitted_at: u64::from_le_bytes(bytes[113..121].try_into()?),
        })
    }

    pub(crate) fn to_bytes(&self) -> [u8; Self::BYTES] {
        let mut bytes = [0; Self::BYTES];
        bytes[0] = 1;
        bytes[1..33].copy_from_slice(&self.issuer);
        bytes[33..65].copy_from_slice(&self.build);
        bytes[65..81].copy_from_slice(self.publisher.as_bytes());
        bytes[81..113].copy_from_slice(&self.signer);
        bytes[113..121].copy_from_slice(&self.admitted_at.to_le_bytes());
        bytes
    }
}

impl CertificateClaims {
    fn validate(&self) -> Result<()> {
        if self.publisher == Id::from_bytes([0; 16])
            || self.issued_at == 0
            || self.expires_at <= self.issued_at
            || self.expires_at - self.issued_at > MAX_CERTIFICATE_LIFETIME_SECS
            || self.revocation_epoch == 0
        {
            return Err("invalid publisher certificate claims".into());
        }
        public_key(&self.issuer)?;
        public_key(&self.signer)?;
        Ok(())
    }
    fn bytes(&self) -> Result<[u8; CLAIM_BYTES]> {
        self.validate()?;
        let mut bytes = [0; CLAIM_BYTES];
        bytes[0] = 1;
        bytes[1..33].copy_from_slice(&self.issuer);
        bytes[33..49].copy_from_slice(self.publisher.as_bytes());
        bytes[49..81].copy_from_slice(&self.signer);
        bytes[81..89].copy_from_slice(&self.issued_at.to_le_bytes());
        bytes[89..97].copy_from_slice(&self.expires_at.to_le_bytes());
        bytes[97..105].copy_from_slice(&self.revocation_epoch.to_le_bytes());
        Ok(bytes)
    }
}

impl PublisherCertificate {
    pub fn signing_bytes(claims: &CertificateClaims) -> Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(CERTIFICATE_DOMAIN.len() + CLAIM_BYTES);
        bytes.extend_from_slice(CERTIFICATE_DOMAIN);
        bytes.extend_from_slice(&claims.bytes()?);
        Ok(bytes)
    }
    pub fn issue(claims: CertificateClaims, issuer_pkcs8: &[u8]) -> Result<Self> {
        let issuer = SigningKey::from_pkcs8_der(issuer_pkcs8)?;
        if issuer.verifying_key().to_bytes() != claims.issuer {
            return Err("publisher certificate issuer key differs".into());
        }
        let signature = issuer.sign(&Self::signing_bytes(&claims)?).to_bytes();
        Ok(Self { claims, signature })
    }
    // fuzz: package_contract
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != CERTIFICATE_BYTES || bytes[0] != 1 {
            return Err("unsupported publisher certificate".into());
        }
        let claims = CertificateClaims {
            issuer: bytes[1..33].try_into()?,
            publisher: Id::from_bytes(bytes[33..49].try_into()?),
            signer: bytes[49..81].try_into()?,
            issued_at: u64::from_le_bytes(bytes[81..89].try_into()?),
            expires_at: u64::from_le_bytes(bytes[89..97].try_into()?),
            revocation_epoch: u64::from_le_bytes(bytes[97..105].try_into()?),
        };
        claims.validate()?;
        Ok(Self { claims, signature: bytes[105..169].try_into()? })
    }
    pub fn to_bytes(&self) -> Result<[u8; CERTIFICATE_BYTES]> {
        let mut bytes = [0; CERTIFICATE_BYTES];
        bytes[..CLAIM_BYTES].copy_from_slice(&self.claims.bytes()?);
        bytes[CLAIM_BYTES..].copy_from_slice(&self.signature);
        Ok(bytes)
    }
    pub fn verify(&self, trust: &PublisherTrust<'_>, now: u64) -> Result<()> {
        self.verify_signature()?;
        let claims = &self.claims;
        if !trust.issuers.contains(&claims.issuer) {
            return Err("publisher certificate issuer is not trusted".into());
        }
        if now < claims.issued_at || now >= claims.expires_at {
            return Err("publisher certificate is not currently valid".into());
        }
        let mut statuses = trust.signers.iter().filter(|status| {
            status.issuer == claims.issuer
                && status.publisher == claims.publisher
                && status.signer == claims.signer
        });
        let status = statuses.next().ok_or("publisher signing key status is unavailable")?;
        if statuses.next().is_some() || status.revoked || status.epoch < claims.revocation_epoch {
            return Err("publisher signing key is revoked or its status is stale".into());
        }
        Ok(())
    }
    pub fn verify_admitted(
        &self,
        trust: &PublisherTrust<'_>,
        admission: &PackageAdmission,
        now: u64,
    ) -> Result<()> {
        let claims = &self.claims;
        if admission.issuer != claims.issuer
            || admission.publisher != claims.publisher
            || admission.signer != claims.signer
            || admission.admitted_at == 0
            || admission.admitted_at > now
        {
            return Err("retained admission differs from publisher certificate".into());
        }
        self.verify(trust, admission.admitted_at)
    }
    fn verify_signature(&self) -> Result<()> {
        verify_ed25519(&self.claims.issuer, &Self::signing_bytes(&self.claims)?, &self.signature)
    }
}

/// A source builder's certificate and protected signing key. Copies of packages
/// retain their existing signature and never pass through this constructor.
pub struct PackageSigner {
    pub certificate: PublisherCertificate,
    key: SigningKey,
}
impl PackageSigner {
    pub fn from_seed(certificate: PublisherCertificate, seed: &[u8; 32]) -> Result<Self> {
        Self::from_key(certificate, SigningKey::from_bytes(seed))
    }
    pub fn new(certificate: PublisherCertificate, signer_pkcs8: &[u8]) -> Result<Self> {
        Self::from_key(certificate, SigningKey::from_pkcs8_der(signer_pkcs8)?)
    }
    fn from_key(certificate: PublisherCertificate, key: SigningKey) -> Result<Self> {
        certificate.verify_signature()?;
        if key.verifying_key().to_bytes() != certificate.claims.signer {
            return Err("package signing key differs from its certificate".into());
        }
        Ok(Self { certificate, key })
    }
    pub(crate) fn declaration(&self, signed_at: u64) -> Result<PackageSignature> {
        if signed_at < self.certificate.claims.issued_at
            || signed_at >= self.certificate.claims.expires_at
        {
            return Err("package signing certificate is not currently valid".into());
        }
        Ok(PackageSignature {
            certificate: self.certificate.clone(),
            signed_at,
            signature: [0; 64],
        })
    }
    pub(crate) fn sign(&self, digest: &[u8; 32]) -> [u8; 64] {
        self.key.sign(&package_message(digest)).to_bytes()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PackageSignature {
    pub certificate: PublisherCertificate,
    pub signed_at: u64,
    pub signature: [u8; 64],
}
impl PackageSignature {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != SIGNATURE_BYTES || &bytes[..MAGIC.len()] != MAGIC {
            return Err("unsupported package signature".into());
        }
        let certificate = PublisherCertificate::from_bytes(&bytes[8..177])?;
        let signed_at = u64::from_le_bytes(bytes[177..185].try_into()?);
        if signed_at < certificate.claims.issued_at || signed_at >= certificate.claims.expires_at {
            return Err("package signature time is outside its certificate".into());
        }
        Ok(Self { certificate, signed_at, signature: bytes[185..249].try_into()? })
    }
    pub fn to_bytes(&self) -> Result<[u8; SIGNATURE_BYTES]> {
        let mut bytes = [0; SIGNATURE_BYTES];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..177].copy_from_slice(&self.certificate.to_bytes()?);
        bytes[177..185].copy_from_slice(&self.signed_at.to_le_bytes());
        bytes[185..249].copy_from_slice(&self.signature);
        Ok(bytes)
    }
    pub fn authenticate(
        &self,
        app_id: &str,
        digest: &[u8; 32],
        trust: &PublisherTrust<'_>,
        now: u64,
    ) -> Result<AppIdentity> {
        self.certificate.verify(trust, now)?;
        if self.signed_at > now {
            return Err("package signature is dated in the future".into());
        }
        verify_ed25519(&self.certificate.claims.signer, &package_message(digest), &self.signature)?;
        let identity =
            AppIdentity { publisher: self.certificate.claims.publisher, app_id: app_id.to_owned() };
        identity.validate()?;
        Ok(identity)
    }

    pub(super) fn authenticate_admitted(
        &self,
        app_id: &str,
        digest: &[u8; 32],
        build: &[u8; 32],
        trust: &PublisherTrust<'_>,
        admission: &PackageAdmission,
        now: u64,
    ) -> Result<AppIdentity> {
        let claims = &self.certificate.claims;
        if admission.build != *build
            || admission.issuer != claims.issuer
            || admission.publisher != claims.publisher
            || admission.signer != claims.signer
            || admission.admitted_at == 0
            || admission.admitted_at > now
        {
            return Err("retained package admission differs from candidate".into());
        }
        // The original validity window is retained; the current signer status
        // still denies every execution after a known permanent revocation.
        self.authenticate(app_id, digest, trust, admission.admitted_at)
    }
}

fn public_key(bytes: &[u8; 32]) -> Result<VerifyingKey> {
    let key = VerifyingKey::from_bytes(bytes)?;
    if key.is_weak() {
        return Err("weak Ed25519 public key".into());
    }
    Ok(key)
}
pub fn verify_ed25519(key: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> Result<()> {
    public_key(key)?.verify_strict(message, &Signature::from_bytes(signature))?;
    Ok(())
}
fn package_message(digest: &[u8; 32]) -> Vec<u8> {
    let mut message = Vec::with_capacity(PACKAGE_DOMAIN.len() + digest.len());
    message.extend_from_slice(PACKAGE_DOMAIN);
    message.extend_from_slice(digest);
    message
}

#[cfg(all(
    feature = "test-support",
    not(target_arch = "wasm32"),
    feature = "local-store",
    feature = "ap-identity"
))]
pub mod testing;
#[cfg(all(
    any(test, feature = "phase1-qualification", feature = "desktop-qualification"),
    not(all(
        feature = "test-support",
        not(target_arch = "wasm32"),
        feature = "local-store",
        feature = "ap-identity"
    ))
))]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
