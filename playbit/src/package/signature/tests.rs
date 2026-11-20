//! Authenticated publisher and byte coverage, independent of online AP state.
#![allow(clippy::unwrap_used, reason = "Tests assert cryptographic fixture construction.")]
use super::*;
use crate::package::{Package, codec};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use std::{collections::BTreeMap, io::Read};

const NOW: u64 = 1_800_000_000;
fn signer(publisher: Id, seed: [u8; 32]) -> PackageSigner {
    let issuer = SigningKey::from_bytes(&[7; 32]);
    let key = SigningKey::from_bytes(&seed);
    let claims = CertificateClaims {
        issuer: issuer.verifying_key().to_bytes(),
        publisher,
        signer: key.verifying_key().to_bytes(),
        issued_at: NOW - 1,
        expires_at: NOW + 100,
        revocation_epoch: 3,
    };
    let certificate =
        PublisherCertificate::issue(claims, issuer.to_pkcs8_der().unwrap().as_bytes()).unwrap();
    PackageSigner::from_seed(certificate, &seed).unwrap()
}
fn trust(signer: &PackageSigner) -> ([u8; 32], TrustedSigner) {
    let claims = &signer.certificate.claims;
    (
        claims.issuer,
        TrustedSigner {
            issuer: claims.issuer,
            publisher: claims.publisher,
            signer: claims.signer,
            epoch: claims.revocation_epoch,
            revoked: false,
        },
    )
}
fn unsigned() -> Package {
    Package::new(
        "test".into(),
        "Example".into(),
        b"\0asm\x01\0\0\0\0\x01\0".to_vec(),
        BTreeMap::from([
            ("asset".into(), b"private seed".to_vec()),
            ("app/extensions/example.notes/readme".into(), b"unknown optional content".to_vec()),
        ]),
    )
    .unwrap()
}
fn signed_bytes(signer: &PackageSigner) -> Vec<u8> {
    let mut bytes = Vec::new();
    unsigned().write_signed(signer, NOW, &mut bytes).unwrap();
    bytes
}

#[test]
fn copied_package_authenticates_original_publisher_and_preserves_every_byte() {
    // Importing unchanged bytes under another runner cannot replace authorship.
    let signer = signer(Id::from_bytes([2; 16]), [8; 32]);
    let bytes = signed_bytes(&signer);
    let (issuer, status) = trust(&signer);
    let trust = PublisherTrust { issuers: &[issuer], signers: &[status] };
    let parsed = Package::from_bytes(&bytes).unwrap();
    assert!(parsed.admit_execution(|_| true).is_err());
    let package = parsed.authenticate(&trust, NOW).unwrap();
    package.admit_execution(|_| true).unwrap();
    assert_eq!(package.identity().unwrap().publisher, Id::from_bytes([2; 16]));
    assert_eq!(package.identity().unwrap().app_id, "test");
    assert_eq!(package.to_bytes().unwrap(), bytes);
    assert_eq!(package.digest(), *blake3::hash(&bytes).as_bytes());
    assert_eq!(
        package.read("app/extensions/example.notes/readme", 0, 100).unwrap(),
        b"unknown optional content"
    );
    assert!(package.write_signed(&signer, NOW, &mut Vec::new()).is_err());
    assert!(unsigned().admit_execution(|_| true).is_err());
}

#[test]
fn package_signature_covers_identity_code_initialization_requirements_and_resources() {
    // Each sampled authority region and arbitrary resource is part of the signed digest.
    let signer = signer(Id::from_bytes([2; 16]), [8; 32]);
    let bytes = signed_bytes(&signer);
    let package = Package::from_bytes(&bytes).unwrap();
    let (issuer, status) = trust(&signer);
    let trust = PublisherTrust { issuers: &[issuer], signers: &[status] };
    let mut offsets = vec![0, 8, 24, 40];
    for entry in &package.index().entries {
        offsets.push(entry.offset as usize);
        offsets.push((entry.offset + entry.length - 1) as usize);
    }
    let signature = package.index().entry(codec::SIGNATURE).unwrap().offset as usize;
    offsets.extend([
        signature + 9,
        signature + 42,
        signature + 60,
        signature + 130,
        signature + 180,
    ]);
    for offset in offsets {
        let mut changed = bytes.clone();
        changed[offset] ^= 1;
        if let Ok(parsed) = Package::from_bytes(&changed) {
            assert!(parsed.authenticate(&trust, NOW).is_err(), "offset {offset}");
        }
    }
}

#[test]
fn certificate_trust_expiry_and_current_signer_status_are_mandatory() {
    // A valid embedded signature cannot authorize an unknown, revoked or stale signer.
    let signer = signer(Id::from_bytes([2; 16]), [8; 32]);
    let (issuer, status) = trust(&signer);
    let certificate = &signer.certificate;
    assert_eq!(
        PublisherCertificate::from_bytes(&certificate.to_bytes().unwrap()).unwrap(),
        *certificate
    );
    for time in [NOW - 2, NOW + 100] {
        assert!(
            certificate
                .verify(
                    &PublisherTrust { issuers: &[issuer], signers: std::slice::from_ref(&status) },
                    time
                )
                .is_err()
        );
    }
    assert!(
        certificate
            .verify(&PublisherTrust { issuers: &[], signers: std::slice::from_ref(&status) }, NOW)
            .is_err()
    );
    assert!(certificate.verify(&PublisherTrust { issuers: &[issuer], signers: &[] }, NOW).is_err());
    let stale = TrustedSigner { epoch: 2, ..status.clone() };
    assert!(
        certificate.verify(&PublisherTrust { issuers: &[issuer], signers: &[stale] }, NOW).is_err()
    );
    let revoked = TrustedSigner { revoked: true, epoch: 4, ..status.clone() };
    assert!(
        certificate
            .verify(&PublisherTrust { issuers: &[issuer], signers: &[revoked] }, NOW)
            .is_err()
    );
    assert!(
        certificate
            .verify(&PublisherTrust { issuers: &[issuer], signers: &[status.clone(), status] }, NOW)
            .is_err()
    );
    let mut forged = certificate.clone();
    forged.claims.publisher = Id::from_bytes([9; 16]);
    assert!(forged.verify_signature().is_err());
    assert!(PublisherCertificate::from_bytes(&[0; CERTIFICATE_BYTES]).is_err());
}

#[test]
fn signer_rotation_keeps_app_identity_and_signed_content_is_streaming_stable() {
    // Distinct certified keys of the same account identify the same app across byte chunks.
    struct Chunked<'a> {
        bytes: &'a [u8],
        limit: usize,
    }
    impl Read for Chunked<'_> {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            let n = output.len().min(self.limit).min(self.bytes.len());
            output[..n].copy_from_slice(&self.bytes[..n]);
            self.bytes = &self.bytes[n..];
            Ok(n)
        }
    }
    let mut identities = Vec::new();
    for seed in [[8; 32], [9; 32]] {
        let signer = signer(Id::from_bytes([2; 16]), seed);
        let bytes = signed_bytes(&signer);
        let (issuer, status) = trust(&signer);
        let trust = PublisherTrust { issuers: &[issuer], signers: &[status] };
        let package = Package::from_bytes(&bytes).unwrap().authenticate(&trust, NOW).unwrap();
        identities.push(package.identity().unwrap().clone());
        let reference = codec::verify(&mut bytes.as_slice()).unwrap();
        for limit in [1, 7, 63, 64, 65] {
            let verified = codec::verify(&mut Chunked { bytes: &bytes, limit }).unwrap();
            assert_eq!(verified.content_digest, reference.content_digest);
            assert_eq!(verified.digest, package.digest());
        }
    }
    assert_eq!(identities[0], identities[1]);
}
