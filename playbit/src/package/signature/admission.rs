//! Portable evidence that an exact public build passed current-certificate
//! admission. The AP issuer must already be pinned independently of the build.
use crate::{
    package::{Result, signature::PackageAdmission},
    store::Id,
};

const DOMAIN: &[u8] = b"app.playbit.package-admission.v1\0";
pub const BUILD_ADMISSION_BYTES: usize = PackageAdmission::BYTES + 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildAdmissionReceipt {
    admission: PackageAdmission,
    signature: [u8; 64],
}

impl BuildAdmissionReceipt {
    fn message(admission: &PackageAdmission) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(DOMAIN.len() + PackageAdmission::BYTES);
        bytes.extend_from_slice(DOMAIN);
        bytes.extend_from_slice(&admission.to_bytes());
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != BUILD_ADMISSION_BYTES {
            return Err("invalid build admission receipt length".into());
        }
        Ok(Self {
            admission: PackageAdmission::from_trusted_bytes(&bytes[..PackageAdmission::BYTES])?,
            signature: bytes[PackageAdmission::BYTES..].try_into()?,
        })
    }

    pub fn to_bytes(&self) -> [u8; BUILD_ADMISSION_BYTES] {
        let mut bytes = [0; BUILD_ADMISSION_BYTES];
        bytes[..PackageAdmission::BYTES].copy_from_slice(&self.admission.to_bytes());
        bytes[PackageAdmission::BYTES..].copy_from_slice(&self.signature);
        bytes
    }

    pub fn verify(
        &self,
        issuers: &[[u8; 32]],
        build: &[u8; 32],
        now: u64,
    ) -> Result<PackageAdmission> {
        let claims = &self.admission;
        if !issuers.contains(&claims.issuer)
            || claims.build != *build
            || claims.publisher == Id::from_bytes([0; 16])
            || claims.admitted_at == 0
            || claims.admitted_at > now
        {
            return Err("untrusted or mismatched build admission receipt".into());
        }
        crate::package::signature::verify_ed25519(
            &claims.issuer,
            &Self::message(claims),
            &self.signature,
        )?;
        Ok(claims.clone())
    }

    #[cfg(feature = "ap-identity")]
    pub fn issue(
        issuer_pkcs8: &[u8],
        build: [u8; 32],
        publisher: Id,
        signer: [u8; 32],
        admitted_at: u64,
    ) -> Result<Self> {
        use ed25519_dalek::{Signer, SigningKey, pkcs8::DecodePrivateKey};
        let issuer = SigningKey::from_pkcs8_der(issuer_pkcs8)?;
        let admission = PackageAdmission {
            issuer: issuer.verifying_key().to_bytes(),
            build,
            publisher,
            signer,
            admitted_at,
        };
        let receipt =
            Self { signature: issuer.sign(&Self::message(&admission)).to_bytes(), admission };
        receipt.verify(&[receipt.admission.issuer], &build, admitted_at)?;
        Ok(receipt)
    }
}

#[cfg(all(test, feature = "ap-identity"))]
mod tests {
    use super::*;
    use ed25519_dalek::{SigningKey, pkcs8::EncodePrivateKey};

    #[test]
    fn admission_binds_every_byte_build_issuer_and_authenticated_time() {
        let issuer = SigningKey::from_bytes(&[31; 32]);
        let public = issuer.verifying_key().to_bytes();
        let build = [41; 32];
        let receipt = BuildAdmissionReceipt::issue(
            issuer.to_pkcs8_der().expect("fixed publisher key encodes").as_bytes(),
            build,
            Id::from_bytes([51; 16]),
            [61; 32],
            100,
        )
        .expect("fixture publisher certificate is issued");
        let bytes = receipt.to_bytes();
        let decoded =
            BuildAdmissionReceipt::from_bytes(&bytes).expect("fixed fixture bytes decode");
        assert_eq!(decoded, receipt);
        assert!(decoded.verify(&[public], &build, 1000).is_ok());
        assert!(decoded.verify(&[], &build, 1000).is_err());
        assert!(decoded.verify(&[public], &[42; 32], 1000).is_err());
        assert!(decoded.verify(&[public], &build, 99).is_err());
        for index in 0..bytes.len() {
            let mut changed = bytes;
            changed[index] ^= 1;
            if let Ok(changed) = BuildAdmissionReceipt::from_bytes(&changed) {
                assert!(changed.verify(&[public], &build, 1000).is_err());
            }
        }
    }
}
