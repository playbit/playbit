//! Client-only encryption for account credential backups. Email authentication
//! authorizes fetching ciphertext; only a saved account key decrypts it.
use crate::store::{Error, Id, Result, random_array};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::aead;

pub const MAX_CIPHERTEXT_BYTES: usize = 256 * 1024;
pub const MAX_PLAINTEXT_BYTES: usize = 2 * 1024 * 1024;
const ENVELOPE_OVERHEAD: usize = 1 + 12 + 16;
const DOMAIN: &[u8] = b"app.playbit.credentials.v1\0";

pub fn encode_ciphertext(bytes: &[u8]) -> Result<String> {
    validate_envelope(bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub fn decode_ciphertext(value: &str) -> Result<Vec<u8>> {
    if value.len() > MAX_CIPHERTEXT_BYTES.div_ceil(3) * 4 {
        return Err(Error::InvalidInput("credential backup is too large"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| Error::InvalidInput("invalid credential backup encoding"))?;
    validate_envelope(&bytes)?;
    Ok(bytes)
}

/// Envelope: version, random 96-bit nonce, ciphertext and 128-bit GCM tag.
/// Version 1 encrypts plaintext; version 2 encrypts bounded zlib data with distinct AAD.
/// AAD binds the ciphertext to the canonical account and the resulting revision.
pub fn seal(account: Id, revision: u64, secret: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>> {
    seal_with_nonce(account, revision, secret, plaintext, random_array()?)
}

fn seal_with_nonce(
    account: Id,
    revision: u64,
    secret: &[u8; 32],
    plaintext: &[u8],
    nonce: [u8; 12],
) -> Result<Vec<u8>> {
    if revision == 0 || plaintext.is_empty() || plaintext.len() > MAX_PLAINTEXT_BYTES {
        return Err(Error::InvalidInput("invalid credential backup size or revision"));
    }
    let version = if plaintext.len() <= MAX_CIPHERTEXT_BYTES - ENVELOPE_OVERHEAD { 1 } else { 2 };
    let mut ciphertext = if version == 1 {
        plaintext.to_vec()
    } else {
        miniz_oxide::deflate::compress_to_vec_zlib(plaintext, 6)
    };
    if ciphertext.len() > MAX_CIPHERTEXT_BYTES - ENVELOPE_OVERHEAD {
        return Err(Error::InvalidInput("credential backup is too large"));
    }
    let key = key(secret)?;
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::from(aad(account, revision, version)),
        &mut ciphertext,
    )
    .map_err(|_| Error::System("credential encryption failed".into()))?;
    let mut envelope = Vec::with_capacity(13 + ciphertext.len());
    envelope.push(version);
    envelope.extend_from_slice(&nonce);
    envelope.extend_from_slice(&ciphertext);
    Ok(envelope)
}

pub fn open(account: Id, revision: u64, secret: &[u8; 32], envelope: &[u8]) -> Result<Vec<u8>> {
    validate_envelope(envelope)?;
    if revision == 0 {
        return Err(Error::InvalidInput("invalid credential backup revision"));
    }
    let nonce: [u8; 12] =
        envelope[1..13].try_into().expect("checked fixed-width field has the destination width");
    let mut ciphertext = envelope[13..].to_vec();
    let plaintext = key(secret)?
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(aad(account, revision, envelope[0])),
            &mut ciphertext,
        )
        .map_err(|_| Error::AccessDenied("account key or credential backup does not match"))?;
    if envelope[0] == 1 {
        Ok(plaintext.to_vec())
    } else {
        miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(plaintext, MAX_PLAINTEXT_BYTES)
            .map_err(|_| Error::InvalidInput("invalid compressed credential backup"))
    }
}

fn aad(account: Id, revision: u64, version: u8) -> Vec<u8> {
    let mut value =
        if version == 1 { DOMAIN.to_vec() } else { b"app.playbit.credentials.v2\0".to_vec() };
    value.extend_from_slice(account.as_bytes());
    value.extend_from_slice(&revision.to_le_bytes());
    value
}

fn key(secret: &[u8; 32]) -> Result<aead::LessSafeKey> {
    aead::UnboundKey::new(&aead::AES_256_GCM, secret)
        .map(aead::LessSafeKey::new)
        .map_err(|_| Error::InvalidInput("invalid account key"))
}

fn validate_envelope(bytes: &[u8]) -> Result<()> {
    if bytes.len() <= ENVELOPE_OVERHEAD
        || bytes.len() > MAX_CIPHERTEXT_BYTES
        || !matches!(bytes[0], 1 | 2)
    {
        return Err(Error::InvalidInput("invalid credential backup envelope"));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn ciphertext_matches_webcrypto_vector() {
        let value: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/recovery-v1.json")).unwrap();
        let account = Id::parse(value["account"].as_str().unwrap()).unwrap();
        let secret: [u8; 32] = (0..32)
            .map(|i| {
                u8::from_str_radix(&value["secret"].as_str().unwrap()[i * 2..i * 2 + 2], 16)
                    .unwrap()
            })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let nonce = std::array::from_fn(|i| 0xa0 + i as u8);
        let plaintext = value["plaintext"].as_str().unwrap().as_bytes();
        let ciphertext = seal_with_nonce(account, 7, &secret, plaintext, nonce).unwrap();
        assert_eq!(encode_ciphertext(&ciphertext).unwrap(), value["ciphertext"]);
        assert_eq!(open(account, 7, &secret, &ciphertext).unwrap(), plaintext);
        assert_eq!(decode_ciphertext(value["ciphertext"].as_str().unwrap()).unwrap(), ciphertext);
    }

    #[test]
    fn compressed_web_fixture_and_native_round_trip_preserve_bounded_plaintext() {
        // Node WebCrypto + zlib fixture independently exercises the v2 reader.
        let value: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/recovery-v2.json")).unwrap();
        let account = Id::parse(value["account"].as_str().unwrap()).unwrap();
        let secret = std::array::from_fn(|n| n as u8);
        let plaintext = value["pattern"]
            .as_str()
            .unwrap()
            .repeat(value["repetitions"].as_u64().unwrap() as usize);
        let fixture = decode_ciphertext(value["ciphertext"].as_str().unwrap()).unwrap();
        assert_eq!(open(account, 7, &secret, &fixture).unwrap(), plaintext.as_bytes());
        let plaintext = plaintext.repeat(2);
        let native = seal(account, 7, &secret, plaintext.as_bytes()).unwrap();
        assert_eq!(native[0], 2);
        assert_eq!(open(account, 7, &secret, &native).unwrap(), plaintext.as_bytes());
        let mut tampered = native;
        tampered[0] = 1;
        assert!(open(account, 7, &secret, &tampered).is_err());
        // A validly encrypted decompression bomb is still outside the reader contract.
        let compressed =
            miniz_oxide::deflate::compress_to_vec_zlib(&vec![0; MAX_PLAINTEXT_BYTES + 1], 6);
        let mut body = compressed;
        key(&secret)
            .unwrap()
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key([0; 12]),
                aead::Aad::from(aad(account, 7, 2)),
                &mut body,
            )
            .unwrap();
        let mut bomb = vec![2];
        bomb.extend_from_slice(&[0; 12]);
        bomb.extend_from_slice(&body);
        assert!(open(account, 7, &secret, &bomb).is_err());
    }

    #[test]
    fn account_revision_secret_nonce_and_content_are_authenticated() {
        let account = Id::from_bytes([5; 16]);
        let secret = [7; 32];
        let ciphertext = seal(account, 1, &secret, b"private credentials").unwrap();
        assert_ne!(ciphertext, seal(account, 1, &secret, b"private credentials").unwrap());
        assert!(open(Id::from_bytes([6; 16]), 1, &secret, &ciphertext).is_err());
        assert!(open(account, 2, &secret, &ciphertext).is_err());
        assert!(open(account, 1, &[8; 32], &ciphertext).is_err());
        for index in 0..ciphertext.len() {
            let mut corrupt = ciphertext.clone();
            corrupt[index] ^= 1;
            assert!(open(account, 1, &secret, &corrupt).is_err(), "tamper byte {index}");
        }
        for end in 0..ciphertext.len() {
            assert!(open(account, 1, &secret, &ciphertext[..end]).is_err());
        }
    }

    #[test]
    fn invalid_or_oversized_envelopes_fail_before_decryption() {
        let account = Id::from_bytes([1; 16]);
        assert!(seal(account, 0, &[0; 32], b"value").is_err());
        assert!(seal(account, 1, &[0; 32], b"").is_err());
        assert!(seal(account, 1, &[0; 32], &vec![0; MAX_PLAINTEXT_BYTES + 1]).is_err());
        assert!(decode_ciphertext("not base64!").is_err());
        assert!(decode_ciphertext(&"A".repeat(MAX_CIPHERTEXT_BYTES * 2)).is_err());
        assert!(decode_ciphertext("AQ==").is_err());
        assert!(encode_ciphertext(&vec![1; MAX_CIPHERTEXT_BYTES + 1]).is_err());
    }
}
