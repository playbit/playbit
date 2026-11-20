//! Authenticated envelopes for host-owned AP identity persistence. The host
//! supplies a secret 256-bit wrapping key, a fresh random nonce for every seal,
//! and a stable device/app context. Never store the wrapping key with ciphertext.
//! This does not provide a KDF for passwords, anti-rollback, atomic storage,
//! enrollment, or protection for other profile records.
use super::ClientIdentity;
use ring::aead;
use std::io::{self, Write};
use zeroize::Zeroizing;

pub const MAX_PLAINTEXT_BYTES: usize = 16 * 1024;
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
pub const MAX_ENVELOPE_BYTES: usize = NONCE_BYTES + MAX_PLAINTEXT_BYTES + TAG_BYTES;
const DOMAIN: &[u8] = b"app.playbit.ap-identity\0";
type Result<T> = std::result::Result<T, &'static str>;

/// Serialize directly into bounded scratch and encrypt in place. The supplied
/// nonce MUST be fresh for this key, including retries and after a restart.
/// Secure entropy and key lifetime are the caller's responsibility.
pub fn seal(
    identity: &ClientIdentity,
    key: &[u8; 32],
    nonce: [u8; NONCE_BYTES],
    context: &[u8; 16],
) -> Result<Vec<u8>> {
    if !identity.has_private_key() {
        return Err("identity has no prepared private key");
    }
    let mut output = Bounded(Zeroizing::new(Vec::with_capacity(MAX_ENVELOPE_BYTES)));
    output.0.extend_from_slice(&nonce);
    serde_json::to_writer(&mut output, identity).map_err(|_| "identity exceeds envelope limit")?;
    let tag = cipher(key)?
        .seal_in_place_separate_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(associated_data(context)),
            &mut output.0[NONCE_BYTES..],
        )
        .map_err(|_| "identity encryption failed")?;
    output.0.extend_from_slice(tag.as_ref());
    assert!(valid_envelope_length(output.0.len()), "bounded writer leaves nonce and tag space");
    Ok(core::mem::take(&mut *output.0))
}

/// Authenticate before JSON decoding. Wrong key/context and altered ciphertext
/// share one error. Admission precedes copying or cryptographic work. Plaintext
/// scratch is zeroed on every return; the returned identity owns its key bytes.
pub fn open(bytes: &[u8], key: &[u8; 32], context: &[u8; 16]) -> Result<ClientIdentity> {
    if !valid_envelope_length(bytes.len()) {
        return Err("invalid identity envelope length");
    }
    let nonce: [u8; NONCE_BYTES] = bytes[..NONCE_BYTES].try_into().expect("admitted nonce width");
    let mut scratch = Zeroizing::new(bytes[NONCE_BYTES..].to_vec());
    let cipher = cipher(key)?;
    let plain = cipher
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(associated_data(context)),
            &mut scratch,
        )
        .map_err(|_| "identity authentication failed")?;
    let identity: ClientIdentity =
        serde_json::from_slice(plain).map_err(|_| "invalid sealed identity")?;
    if !identity.has_private_key() {
        return Err("identity has no prepared private key");
    }
    Ok(identity)
}

fn cipher(key: &[u8; 32]) -> Result<aead::LessSafeKey> {
    aead::UnboundKey::new(&aead::AES_256_GCM, key)
        .map(aead::LessSafeKey::new)
        .map_err(|_| "identity key rejected")
}
fn associated_data(context: &[u8; 16]) -> [u8; DOMAIN.len() + 16] {
    let mut data = [0; DOMAIN.len() + 16];
    data[..DOMAIN.len()].copy_from_slice(DOMAIN);
    data[DOMAIN.len()..].copy_from_slice(context);
    data
}
fn valid_envelope_length(len: usize) -> bool {
    len > NONCE_BYTES + TAG_BYTES && len <= MAX_ENVELOPE_BYTES
}
fn admit_write(current: usize, additional: usize) -> bool {
    (NONCE_BYTES..=NONCE_BYTES + MAX_PLAINTEXT_BYTES).contains(&current)
        && additional <= NONCE_BYTES + MAX_PLAINTEXT_BYTES - current
}
struct Bounded(Zeroizing<Vec<u8>>);
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !admit_write(self.0.len(), bytes.len()) {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
