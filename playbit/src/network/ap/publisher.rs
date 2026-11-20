//! Signed publisher-key status for offline package admission. Issuer keys come
//! from the host's pinned AP trust configuration, never from these bytes alone.
//! A revoked key stays revoked. Offline verification uses the last authenticated
//! snapshot and cannot discover a revocation made while disconnected.
use crate::store::Id;

pub use crate::package::signature::admission::{BUILD_ADMISSION_BYTES, BuildAdmissionReceipt};

pub const CERTIFICATE_SECONDS: u64 = 365 * 24 * 60 * 60;
#[cfg(feature = "ap-identity")]
pub use crate::package::signature::status::issuer_key_from_certificate;
pub use crate::package::signature::status::{KeyStatus, KeyStatusClaims, STATUS_BYTES};
const REQUEST_DOMAIN: &[u8] = b"app.playbit.publisher-certificate.request.v1\0";

pub fn certificate_request_message(publisher: Id, signer: &[u8; 32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(REQUEST_DOMAIN.len() + 16 + 32);
    bytes.extend_from_slice(REQUEST_DOMAIN);
    bytes.extend_from_slice(publisher.as_bytes());
    bytes.extend_from_slice(signer);
    bytes
}

#[cfg(all(test, feature = "ap-identity"))]
mod tests {
    use super::*;
    use ring::signature::KeyPair;

    #[test]
    fn signed_status_binds_every_byte_and_rejects_rollback() {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("issuer");
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(&key.serialize_der()).expect("key");
        let issuer: [u8; 32] = pair.public_key().as_ref().try_into().expect("key width");
        let claims = KeyStatusClaims {
            issuer,
            publisher: Id::from_bytes([1; 16]),
            signer: [2; 32],
            epoch: 1,
            checked_at: 100,
            revoked: false,
        };
        let status = KeyStatus::issue(claims.clone(), &key.serialize_der()).expect("status");
        let bytes = status.to_bytes();
        assert_eq!(KeyStatus::from_bytes(&bytes).expect("decode"), status);
        status.verify(&[issuer], 100).expect("trusted status");
        assert!(status.verify(&[], 100).is_err());
        assert!(status.verify(&[issuer], 99).is_err());
        for index in 0..bytes.len() {
            let mut changed = bytes;
            changed[index] ^= 1;
            if let Ok(status) = KeyStatus::from_bytes(&changed) {
                assert!(status.verify(&[issuer], 100).is_err());
            }
        }
        let revoked = KeyStatus::issue(
            KeyStatusClaims { epoch: 2, checked_at: 101, revoked: true, ..claims },
            &key.serialize_der(),
        )
        .expect("revoked status");
        revoked.admit_update(&status).expect("advance");
        assert!(status.admit_update(&revoked).is_err());
    }
}
