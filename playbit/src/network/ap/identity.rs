//! AP identity and signing state, independent of transport and desktop storage.
//! Hosts own secure entropy and protected persistence. Serialization is the same
//! private credential shape used by the native client; it is not encryption.
use super::ClientError;
use rcgen::{KeyPair, PKCS_ED25519, PublicKeyData};
use rustls::pki_types::PrivatePkcs8KeyDer;
use serde::{Deserialize, Serialize};
#[cfg(feature = "local-store")]
mod file;
#[cfg(feature = "local-store")]
mod metadata;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientIdentity {
    pub email: String,
    pub instance_name: String,
    pub instance_id: Vec<u8>,
    pub auth_token: Vec<u8>,
    private_key_der: Vec<u8>,
    pub ca_certificate: Vec<u8>,
    pub instance_certificate: Vec<u8>,
}

impl ClientIdentity {
    pub fn new(email: impl Into<String>, instance_name: impl Into<String>) -> Self {
        Self {
            email: email.into(),
            instance_name: instance_name.into(),
            private_key_der: KeyPair::generate_for(&PKCS_ED25519)
                .expect("Ed25519 key generation failed")
                .serialize_der(),
            ..Self::default()
        }
    }

    /// Import the local account's persisted Ed25519 seed before any enrollment request.
    /// Save this identity before sending the code so a lost response is safely retryable.
    pub fn from_seed(
        email: impl Into<String>,
        instance_name: impl Into<String>,
        seed: [u8; 32],
    ) -> Result<Self, ClientError> {
        // RFC 8410 PKCS#8: id-Ed25519 with a nested 32-byte private OCTET STRING.
        let mut private_key_der = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        private_key_der.extend_from_slice(&seed);
        KeyPair::from_pkcs8_der_and_sign_algo(
            &PrivatePkcs8KeyDer::from(private_key_der.as_slice()),
            &PKCS_ED25519,
        )?;
        Ok(Self {
            email: email.into(),
            instance_name: instance_name.into(),
            private_key_der,
            ..Self::default()
        })
    }

    /// Public device key for matching saved AP credentials to a protected profile.
    /// This never generates a key when persisted material is missing or invalid.
    pub fn public_key(&self) -> Result<[u8; 32], ClientError> {
        let der = PrivatePkcs8KeyDer::from(self.private_key_der.as_slice());
        let key = KeyPair::from_pkcs8_der_and_sign_algo(&der, &PKCS_ED25519)?;
        key.public_key_raw().try_into().map_err(|_| ClientError::InvalidPreissuedIdentity)
    }

    /// Whether the host has prepared key material for an enrollment attempt.
    /// This says nothing about protected persistence; the host must provide it.
    pub fn has_private_key(&self) -> bool {
        !self.private_key_der.is_empty()
    }

    /// Account identity from the service-issued token (available after enrollment).
    pub fn account_id(&self) -> Result<[u8; 16], ClientError> {
        if self.auth_token.len() != 72 {
            return Err(ClientError::InvalidAuthenticationToken);
        }
        Ok(self.auth_token[..16]
            .try_into()
            .expect("checked fixed-width field has the destination width"))
    }

    /// Imports one complete identity issued for an unattended AP instance.
    ///
    /// The caller supplies the exact service-held Ed25519 keypair and issued
    /// credential. The private key is verified against its SPKI before it can
    /// be persisted or used by the existing-only connection path.
    pub fn from_preissued(
        instance_name: impl Into<String>,
        instance_id: Vec<u8>,
        auth_token: Vec<u8>,
        private_key_der: Vec<u8>,
        public_key_der: &[u8],
        ca_certificate: Vec<u8>,
        instance_certificate: Vec<u8>,
    ) -> Result<Self, ClientError> {
        let der = PrivatePkcs8KeyDer::from(private_key_der.as_slice());
        let key = KeyPair::from_pkcs8_der_and_sign_algo(&der, &PKCS_ED25519)
            .map_err(|_| ClientError::InvalidPreissuedIdentity)?;
        if key.subject_public_key_info() != public_key_der {
            return Err(ClientError::InvalidPreissuedIdentity);
        }
        let identity = Self {
            email: String::new(),
            instance_name: instance_name.into(),
            instance_id,
            auth_token,
            private_key_der,
            ca_certificate,
            instance_certificate,
        };
        identity.validate_preissued()?;
        Ok(identity)
    }

    pub fn validate_preissued(&self) -> Result<(), ClientError> {
        self.validate_authenticated()?;
        let der = PrivatePkcs8KeyDer::from(self.private_key_der.as_slice());
        KeyPair::from_pkcs8_der_and_sign_algo(&der, &PKCS_ED25519)
            .map_err(|_| ClientError::InvalidPreissuedIdentity)?;
        Ok(())
    }

    /// Build the ordinary signed Ed25519 enrollment CSR from this saved key.
    /// The host must persist newly created identity before sending enrollment,
    /// and maintain its platform's secure entropy preconditions during crypto.
    pub fn certificate_request(&mut self) -> Result<rcgen::CertificateSigningRequest, ClientError> {
        Ok(rcgen::CertificateParams::default().serialize_request(&self.signing_key()?)?)
    }

    pub(super) fn validate_authenticated(&self) -> Result<(), ClientError> {
        if self.auth_token.is_empty() {
            return Err(ClientError::MissingIdentityField("auth token"));
        }
        if self.instance_id.is_empty() {
            return Err(ClientError::MissingIdentityField("instance ID"));
        }
        if self.private_key_der.is_empty() {
            return Err(ClientError::MissingIdentityField("private key"));
        }
        if self.ca_certificate.is_empty() {
            return Err(ClientError::MissingIdentityField("CA certificate"));
        }
        if self.instance_certificate.is_empty() {
            return Err(ClientError::MissingIdentityField("instance certificate"));
        }
        Ok(())
    }

    fn signing_key(&mut self) -> Result<KeyPair, ClientError> {
        if self.private_key_der.is_empty() {
            let key = KeyPair::generate_for(&PKCS_ED25519)?;
            self.private_key_der = key.serialize_der();
            return Ok(key);
        }
        let der = PrivatePkcs8KeyDer::from(self.private_key_der.as_slice());
        Ok(KeyPair::from_pkcs8_der_and_sign_algo(&der, &PKCS_ED25519)?)
    }
}

#[cfg(all(test, feature = "local-store"))]
#[path = "identity/file_tests.rs"]
mod file_tests;
#[cfg(test)]
#[path = "identity/tests.rs"]
mod tests;
