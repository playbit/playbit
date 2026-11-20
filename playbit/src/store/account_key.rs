//! A 128-bit random account seed. Only domain-separated public proofs leave the client.
use crate::store::{Error, Id, Result};
use ring::{
    hkdf,
    signature::{Ed25519KeyPair, KeyPair},
};

pub type AccountKey = [u8; 16];
pub const TEXT_BYTES: usize = 23;
const SALT: &[u8] = b"app.playbit.account-key.v1";

pub fn generate() -> Result<AccountKey> {
    super::random_array()
}
pub fn format(seed: &AccountKey) -> String {
    format!("a{}", super::session_code(seed))
}
// fuzz: store_contract
pub fn parse(text: &str) -> Result<AccountKey> {
    if text.len() > 1024 {
        return Err(Error::InvalidInput("account key is too long"));
    }
    let text = text.trim();
    if text.len() != TEXT_BYTES || !text.starts_with('a') {
        return Err(Error::InvalidInput("enter the complete account key, starting with a"));
    }
    super::parse_session_code(&text[1..]).map_err(|_| Error::InvalidInput("invalid account key"))
}
fn derive(seed: &AccountKey, salt: &[u8], info: &[u8]) -> Result<[u8; 32]> {
    let root = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(seed);
    let mut bytes = [0; 32];
    root.expand(&[info], hkdf::HKDF_SHA256)
        .map_err(|_| Error::Internal("account key derivation expansion failed"))?
        .fill(&mut bytes)
        .map_err(|_| Error::Internal("account key derivation output failed"))?;
    Ok(bytes)
}
pub fn backup_key(account: Id, seed: &AccountKey) -> Result<[u8; 32]> {
    derive(seed, account.as_bytes(), b"app.playbit.account.backup.v1")
}
fn signing_key(seed: &AccountKey) -> Result<Ed25519KeyPair> {
    Ed25519KeyPair::from_seed_unchecked(&derive(seed, SALT, b"app.playbit.account.signing.v1")?)
        .map_err(|_| Error::Internal("could not construct the account signing key"))
}
pub fn public_key(seed: &AccountKey) -> Result<[u8; 32]> {
    Ok(signing_key(seed)?.public_key().as_ref().try_into().expect("Ed25519 public key is 32 bytes"))
}
pub fn sign(seed: &AccountKey, message: &[u8]) -> Result<[u8; 64]> {
    Ok(signing_key(seed)?.sign(message).as_ref().try_into().expect("Ed25519 signature is 64 bytes"))
}
/// The complete request binds the email, instance name and signed device CSR.
pub fn login_message(nonce: &[u8; 32], request: &[u8]) -> Vec<u8> {
    let mut message = b"app.playbit.account.login.v1\0".to_vec();
    message.extend_from_slice(nonce);
    message.extend_from_slice(request);
    message
}
/// A backup signature also binds its account, operation identity and revision.
pub fn backup_message(account: Id, operation: Id, revision: u64, ciphertext: &str) -> Vec<u8> {
    let mut message = b"app.playbit.account.backup.write.v1\0".to_vec();
    message.extend_from_slice(account.as_bytes());
    message.extend_from_slice(operation.as_bytes());
    message.extend_from_slice(&revision.to_le_bytes());
    message.extend_from_slice(ciphertext.as_bytes());
    message
}
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert fixture operations.")]
mod tests {
    use super::*;
    #[test]
    fn account_text_preserves_all_seed_bits() {
        // Fixed width preserves leading zeroes; type/version and overflow fail closed.
        for seed in [[0; 16], [255; 16], generate().unwrap()] {
            let text = format(&seed);
            assert_eq!(text.len(), TEXT_BYTES);
            assert_eq!(parse(&text).unwrap(), seed);
            assert_eq!(parse(&format!("  {text}\n")).unwrap(), seed);
        }
        for text in [
            "",
            "a",
            "a0000000000000000000000",
            "azzzzzzzzzzzzzzzzzzzzzz",
            "b1111111111111111111111",
        ] {
            assert!(parse(text).is_err());
        }
    }
    #[test]
    fn authentication_proofs_bind_every_request_byte() {
        // Altering the challenge or any request field invalidates the signature.
        let seed = [7; 16];
        let message = login_message(&[9; 32], b"email and CSR");
        let public = public_key(&seed).unwrap();
        let verifier = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public);
        let signature = sign(&seed, &message).unwrap();
        verifier.verify(&message, &signature).unwrap();
        for i in 0..message.len() {
            let mut changed = message.clone();
            changed[i] ^= 1;
            assert!(verifier.verify(&changed, &signature).is_err());
        }
        assert_ne!(backup_key(Id::from_bytes([1; 16]), &seed).unwrap(), public);
    }
    #[test]
    fn webcrypto_derivations_and_signatures_match() {
        // Independent Rust and WebCrypto implementations agree on every protocol byte.
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/account-key-v1.json")).unwrap();
        let bytes = |field: &str| -> Vec<u8> {
            let text = v[field].as_str().unwrap();
            (0..text.len() / 2)
                .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).unwrap())
                .collect()
        };
        let seed = bytes("seed").try_into().unwrap();
        assert_eq!(format(&seed), v["text"]);
        assert_eq!(public_key(&seed).unwrap().as_slice(), bytes("public_key"));
        let account = Id::parse(v["account"].as_str().unwrap()).unwrap();
        assert_eq!(backup_key(account, &seed).unwrap().as_slice(), bytes("backup_key"));
        let message = login_message(&bytes("nonce").try_into().unwrap(), &bytes("request"));
        assert_eq!(sign(&seed, &message).unwrap().as_slice(), bytes("login_signature"));
        let message = backup_message(
            account,
            Id::parse(v["operation"].as_str().unwrap()).unwrap(),
            3,
            v["ciphertext"].as_str().unwrap(),
        );
        assert_eq!(sign(&seed, &message).unwrap().as_slice(), bytes("backup_signature"));
    }
}
