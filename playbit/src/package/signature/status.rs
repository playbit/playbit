//! Signed publisher-key state authenticated by an independently pinned AP issuer.
use crate::store::Id;

pub const STATUS_BYTES: usize = 162;
const CLAIMS_BYTES: usize = STATUS_BYTES - 64;
const STATUS_DOMAIN: &[u8] = b"app.playbit.publisher-key-status.v1\0";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyStatusClaims {
    pub issuer: [u8; 32],
    pub publisher: Id,
    pub signer: [u8; 32],
    /// Zero means unknown, one registered, two permanently revoked.
    pub epoch: u64,
    pub checked_at: u64,
    pub revoked: bool,
}

impl KeyStatusClaims {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.issuer == [0; 32]
            || self.publisher == Id::from_bytes([0; 16])
            || self.signer == [0; 32]
            || self.epoch > 2
            || self.revoked != (self.epoch == 2)
            || self.checked_at == 0
        {
            return Err("invalid publisher-key status claims");
        }
        Ok(())
    }

    fn encode(&self) -> [u8; CLAIMS_BYTES] {
        let mut bytes = [0; CLAIMS_BYTES];
        bytes[0] = 1;
        bytes[1..33].copy_from_slice(&self.issuer);
        bytes[33..49].copy_from_slice(self.publisher.as_bytes());
        bytes[49..81].copy_from_slice(&self.signer);
        bytes[81..89].copy_from_slice(&self.epoch.to_le_bytes());
        bytes[89..97].copy_from_slice(&self.checked_at.to_le_bytes());
        bytes[97] = u8::from(self.revoked);
        bytes
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyStatus {
    pub claims: KeyStatusClaims,
    signature: [u8; 64],
}

impl KeyStatus {
    pub fn signing_bytes(claims: &KeyStatusClaims) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(STATUS_DOMAIN.len() + CLAIMS_BYTES);
        bytes.extend_from_slice(STATUS_DOMAIN);
        bytes.extend_from_slice(&claims.encode());
        bytes
    }

    pub fn to_bytes(&self) -> [u8; STATUS_BYTES] {
        let mut bytes = [0; STATUS_BYTES];
        bytes[..CLAIMS_BYTES].copy_from_slice(&self.claims.encode());
        bytes[CLAIMS_BYTES..].copy_from_slice(&self.signature);
        bytes
    }

    // fuzz: ap_codec
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() != STATUS_BYTES || bytes[0] != 1 || bytes[97] > 1 {
            return Err("unsupported publisher-key status");
        }
        let claims = KeyStatusClaims {
            issuer: bytes[1..33].try_into().expect("checked status width"),
            publisher: Id::from_bytes(bytes[33..49].try_into().expect("checked status width")),
            signer: bytes[49..81].try_into().expect("checked status width"),
            epoch: u64::from_le_bytes(bytes[81..89].try_into().expect("checked status width")),
            checked_at: u64::from_le_bytes(bytes[89..97].try_into().expect("checked status width")),
            revoked: bytes[97] == 1,
        };
        claims.validate()?;
        Ok(Self {
            claims,
            signature: bytes[CLAIMS_BYTES..].try_into().expect("checked status signature width"),
        })
    }

    pub fn verify_with(
        &self,
        trusted_issuers: &[[u8; 32]],
        now: u64,
        verify: impl FnOnce(&[u8; 32], &[u8], &[u8; 64]) -> bool,
    ) -> Result<crate::package::signature::TrustedSigner, &'static str> {
        self.claims.validate()?;
        if !trusted_issuers.contains(&self.claims.issuer) {
            return Err("untrusted publisher-key status issuer");
        }
        if self.claims.checked_at > now {
            return Err("publisher-key status is from the future");
        }
        if !verify(&self.claims.issuer, &Self::signing_bytes(&self.claims), &self.signature) {
            return Err("invalid publisher-key status signature");
        }
        Ok(crate::package::signature::TrustedSigner {
            issuer: self.claims.issuer,
            publisher: self.claims.publisher,
            signer: self.claims.signer,
            epoch: self.claims.epoch,
            revoked: self.claims.revoked,
        })
    }

    /// Call after signature verification and before replacing a saved snapshot.
    pub fn admit_update(&self, previous: &Self) -> Result<(), &'static str> {
        let old = &previous.claims;
        let next = &self.claims;
        if old.publisher != next.publisher || old.signer != next.signer {
            return Err("publisher-key status binding changed");
        }
        if next.epoch < old.epoch || next.checked_at < old.checked_at {
            return Err("publisher-key status rollback");
        }
        if old.revoked && !next.revoked {
            return Err("publisher-key revocation is permanent");
        }
        Ok(())
    }

    #[cfg(any(feature = "ap-identity", all(test, feature = "local-store")))]
    pub fn issue(claims: KeyStatusClaims, issuer_pkcs8: &[u8]) -> crate::package::Result<Self> {
        use ring::signature::KeyPair;
        claims.validate()?;
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(issuer_pkcs8)
            .map_err(|_| "invalid publisher issuer key")?;
        if pair.public_key().as_ref() != claims.issuer {
            return Err("publisher issuer key differs from claims".into());
        }
        let signature = pair
            .sign(&Self::signing_bytes(&claims))
            .as_ref()
            .try_into()
            .expect("Ed25519 signature is 64 bytes");
        Ok(Self { claims, signature })
    }

    pub fn verify(
        &self,
        trusted_issuers: &[[u8; 32]],
        now: u64,
    ) -> Result<crate::package::signature::TrustedSigner, &'static str> {
        self.verify_with(trusted_issuers, now, |key, message, signature| {
            crate::package::signature::verify_ed25519(key, message, signature).is_ok()
        })
    }
}

/// Extract only from a CA certificate already pinned by successful AP
/// authentication. Parsing an embedded package certificate never creates trust.
#[cfg(any(target_arch = "wasm32", feature = "ap-identity"))]
pub fn issuer_key_from_certificate(certificate: &[u8]) -> crate::package::Result<[u8; 32]> {
    use x509_parser::prelude::FromDer;
    const MAX_CA_BYTES: usize = 4096;
    if certificate.is_empty() || certificate.len() > MAX_CA_BYTES {
        return Err("invalid pinned AP publisher certificate".into());
    }
    let (remaining, certificate) = x509_parser::certificate::X509Certificate::from_der(certificate)
        .map_err(|_| "invalid pinned AP publisher certificate")?;
    let public = certificate.public_key();
    if !remaining.is_empty()
        || public.algorithm.algorithm.to_id_string() != "1.3.101.112"
        || public.algorithm.parameters.is_some()
        || public.subject_public_key.unused_bits != 0
    {
        return Err("pinned AP publisher issuer must be Ed25519".into());
    }
    let raw: [u8; 32] = public.subject_public_key.data.as_ref().try_into()?;
    let key = ed25519_dalek::VerifyingKey::from_bytes(&raw)?;
    if key.is_weak() {
        return Err("invalid pinned AP publisher issuer key".into());
    }
    Ok(raw)
}
