//! Opaque account records carried by ordinary kvdb replication.
//!
//! The AP knows record keys and clocks but never receives the recovery secret.
//! Authentication covers those keys and clocks as well as the ciphertext. An
//! encrypted record is not a volume database grant or a device signing key.
mod format;
mod record;
use crate::store::{Error, Id, Result};
pub use format::UNSUPPORTED;
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub(crate) use format::{is_unsupported, unsupported};
use kvdb::Clock;
pub use record::{Credentials, Record, validate_display_name};

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub(crate) fn decode_plaintext(plain: &[u8]) -> Result<format::Decoded> {
    format::Frame::decode(plain)
}

pub const MAX_PLAINTEXT_BYTES: usize = 48 * 1024;
pub const ENVELOPE_OVERHEAD: usize = 29;
pub const MAX_ENVELOPE_BYTES: usize = MAX_PLAINTEXT_BYTES + ENVELOPE_OVERHEAD;
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
const DOMAIN: &[u8] = b"app.playbit.account.record.v1\0";
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
const ENCRYPTION_INFO: &[u8] = b"app.playbit.account.encryption.v1";
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
const LOOKUP_INFO: &[u8] = b"app.playbit.account.lookup.v1";

/// Public tree hashing parameters, not an encryption key. Each account owns
/// a separate ordinary kvdb; record envelopes provide client-only authenticity.
pub fn hash_key(account: Id) -> [u8; 32] {
    blake3::derive_key("app.playbit.account.tree.v1", account.as_bytes())
}

/// Shape checks available to the AP. Only an unlocked client can authenticate
/// the contents. Plain kvdb tombstones are forbidden: deletion must itself be
/// represented by an authenticated encrypted record at the application layer.
pub fn validate_envelope(key: &[u8], clock: Clock, value: &[u8]) -> Result<()> {
    if key.len() != 32
        || clock.is_zero()
        || clock.as_u64() > u64::MAX - 3
        || !(ENVELOPE_OVERHEAD + 1..=MAX_ENVELOPE_BYTES).contains(&value.len())
        || value[0] != 1
    {
        return Err(Error::InvalidInput("invalid encrypted account record"));
    }
    Ok(())
}

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
fn aad(account: Id, key: &[u8], clock: Clock) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(DOMAIN.len() + 16 + 32 + 8);
    bytes.extend_from_slice(DOMAIN);
    bytes.extend_from_slice(account.as_bytes());
    bytes.extend_from_slice(key);
    bytes.extend_from_slice(&clock.as_u64().to_le_bytes());
    bytes
}

/// Validate all entries before opening a destination write transaction. A
/// rejected tail cannot leave an accepted prefix durably installed.
pub fn validate_delta(bytes: &[u8]) -> Result<()> {
    inspect_delta(bytes, |_, _, _| Ok(()))
}

pub(crate) fn inspect_delta(
    bytes: &[u8],
    mut inspect: impl FnMut(&[u8], Clock, &[u8]) -> Result<()>,
) -> Result<()> {
    let mut failure = None;
    kvdb::sync::visit_delta(bytes, crate::store::MAX_REPLICA_DELTA_BYTES, |key, state| {
        if failure.is_some() {
            return;
        }
        let result = match state {
            kvdb::sync::EntState::Present { clock, value } => {
                validate_envelope(key, clock, value).and_then(|()| inspect(key, clock, value))
            }
            _ => Err(Error::InvalidInput("account deletion requires an encrypted record")),
        };
        failure = result.err();
    })?;
    failure.map_or(Ok(()), Err)
}

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub struct Cipher {
    account: Id,
    encryption: ring::aead::LessSafeKey,
    lookup: ring::hmac::Key,
}

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
impl Cipher {
    /// HKDF-SHA256, account ID as salt, with separate encryption/lookup domains.
    /// The same saved recovery secret is valid on each enrolled device. Device
    /// authentication seeds must never be supplied here or replicated as data.
    pub fn new(account: Id, recovery_secret: &[u8; 16]) -> Result<Self> {
        use ring::{aead, hkdf, hmac};
        let root = hkdf::Salt::new(hkdf::HKDF_SHA256, account.as_bytes()).extract(recovery_secret);
        let derive = |info: &[u8]| -> Result<[u8; 32]> {
            let mut key = [0; 32];
            root.expand(&[info], hkdf::HKDF_SHA256)
                .map_err(|_| Error::Internal("account key derivation expansion failed"))?
                .fill(&mut key)
                .map_err(|_| Error::Internal("account key derivation output failed"))?;
            Ok(key)
        };
        Ok(Self {
            account,
            encryption: aead::LessSafeKey::new(
                aead::UnboundKey::new(&aead::AES_256_GCM, &derive(ENCRYPTION_INFO)?).map_err(
                    |_| Error::Internal("could not construct the account encryption key"),
                )?,
            ),
            lookup: hmac::Key::new(hmac::HMAC_SHA256, &derive(LOOKUP_INFO)?),
        })
    }

    pub fn record_key(&self, logical_key: &[u8]) -> Result<[u8; 32]> {
        if logical_key.is_empty() || logical_key.len() > kvdb::KEY_SIZE_MAX {
            return Err(Error::InvalidInput("invalid account record key"));
        }
        Ok(ring::hmac::sign(&self.lookup, logical_key)
            .as_ref()
            .try_into()
            .expect("checked fixed-width field has the destination width"))
    }

    pub fn seal(&self, key: &[u8; 32], clock: Clock, plaintext: &[u8]) -> Result<Vec<u8>> {
        self.seal_with_nonce(key, clock, plaintext, crate::store::random_array()?)
    }

    fn seal_with_nonce(
        &self,
        key: &[u8; 32],
        clock: Clock,
        plaintext: &[u8],
        nonce: [u8; 12],
    ) -> Result<Vec<u8>> {
        use ring::aead;
        if plaintext.is_empty()
            || plaintext.len() > MAX_PLAINTEXT_BYTES
            || clock.is_zero()
            || clock.as_u64() > u64::MAX - 3
        {
            return Err(Error::InvalidInput("invalid account record size or clock"));
        }
        let _timing = crate::trace::Span::new("account.seal");
        let mut value = Vec::with_capacity(ENVELOPE_OVERHEAD + plaintext.len());
        value.push(1);
        value.extend_from_slice(&nonce);
        value.extend_from_slice(plaintext);
        let tag = self
            .encryption
            .seal_in_place_separate_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(aad(self.account, key, clock)),
                &mut value[13..],
            )
            .map_err(|_| Error::System("account record encryption failed".into()))?;
        value.extend_from_slice(tag.as_ref());
        Ok(value)
    }

    pub fn open(&self, key: &[u8], clock: Clock, value: &[u8]) -> Result<Vec<u8>> {
        use ring::aead;
        validate_envelope(key, clock, value)?;
        let _timing = crate::trace::Span::new("account.open");
        let mut plaintext = value[13..].to_vec();
        let length = self
            .encryption
            .open_in_place(
                aead::Nonce::assume_unique_for_key(
                    value[1..13]
                        .try_into()
                        .expect("checked fixed-width field has the destination width"),
                ),
                aead::Aad::from(aad(self.account, key, clock)),
                &mut plaintext,
            )
            .map_err(|_| Error::AccessDenied("account record authentication failed"))?
            .len();
        plaintext.truncate(length);
        Ok(plaintext)
    }

    pub fn validate_delta(&self, bytes: &[u8]) -> Result<()> {
        inspect_delta(bytes, |key, clock, value| self.open(key, clock, value).map(|_| ()))
    }

    /// Authenticate the entire publication before mutation, then use ordinary
    /// kvdb conflict resolution and durability. Application records still need
    /// their typed validation before exposing credentials or consent locally.
    pub fn apply_delta<S: kvdb::Storage>(
        &self,
        database: &kvdb::Db<S>,
        bytes: &[u8],
        wall_unix_secs: u64,
    ) -> Result<kvdb::SyncOutcome> {
        if database.meta().hash_key != hash_key(self.account) {
            return Err(Error::AccessDenied("account replica binding"));
        }
        self.validate_delta(bytes)?;
        let mut receiver = kvdb::DeltaReceiver::new(
            database,
            kvdb::Lww,
            wall_unix_secs,
            crate::store::MAX_REPLICA_DELTA_BYTES,
        )?;
        receiver.process(bytes)?;
        Ok(receiver.finish()?)
    }
}

#[cfg(test)]
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use kvdb::{CommittedDelta, CreateOptions, Db, DeltaReceiver, Lww, storage::mem::MemStorage};

    fn unhex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn native_matches_web_crypto_vector_and_authenticates_record_identity() {
        let vector: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/account-record-v1.json"))
                .unwrap();
        let account = Id::parse(vector["account"].as_str().unwrap()).unwrap();
        let secret: [u8; 16] = unhex(vector["secret"].as_str().unwrap()).try_into().unwrap();
        let nonce = unhex(vector["nonce"].as_str().unwrap()).try_into().unwrap();
        let clock = Clock::from_u64(vector["clock"].as_str().unwrap().parse().unwrap());
        let cipher = Cipher::new(account, &secret).unwrap();
        let key = cipher.record_key(vector["logical_key"].as_str().unwrap().as_bytes()).unwrap();
        let value = unhex(vector["envelope"].as_str().unwrap());
        let plain = vector["plaintext"].as_str().unwrap().as_bytes();
        assert_eq!(key.as_slice(), unhex(vector["key"].as_str().unwrap()));
        assert_eq!(cipher.seal_with_nonce(&key, clock, plain, nonce).unwrap(), value);
        assert_eq!(cipher.open(&key, clock, &value).unwrap(), plain);
        assert_ne!(cipher.seal(&key, clock, plain).unwrap(), value);
        assert!(
            Cipher::new(Id::from_bytes([4; 16]), &secret)
                .unwrap()
                .open(&key, clock, &value)
                .is_err()
        );
        assert!(Cipher::new(account, &[2; 16]).unwrap().open(&key, clock, &value).is_err());
        assert!(cipher.open(&[2; 32], clock, &value).is_err());
        assert!(cipher.open(&key, clock.incr(0).unwrap(), &value).is_err());
        for index in 0..value.len() {
            let mut corrupt = value.clone();
            corrupt[index] ^= 1;
            assert!(cipher.open(&key, clock, &corrupt).is_err());
            assert!(cipher.open(&key, clock, &value[..index]).is_err());
        }
        for clock in [
            Clock::ZERO,
            Clock::from_u64(u64::MAX),
            Clock::from_u64(u64::MAX - 1),
            Clock::from_u64(u64::MAX - 2),
        ] {
            assert!(cipher.seal(&key, clock, plain).is_err());
            assert!(cipher.open(&key, clock, &value).is_err());
        }
        assert!(cipher.record_key(&[]).is_err());
        assert!(cipher.record_key(&[1; 512]).is_err());
        assert!(cipher.seal(&key, clock, &[]).is_err());
        assert!(cipher.seal(&key, clock, &vec![1; MAX_PLAINTEXT_BYTES + 1]).is_err());
        let large = cipher.seal(&key, clock, &vec![1; MAX_PLAINTEXT_BYTES]).unwrap();
        assert_eq!(cipher.open(&key, clock, &large).unwrap(), vec![1; MAX_PLAINTEXT_BYTES]);
    }

    #[test]
    fn actual_kvdb_delta_preserves_authenticated_clocks_through_relay_and_replay() {
        let account = Id::from_bytes([1; 16]);
        let cipher = Cipher::new(account, &[7; 16]).unwrap();
        let create = |id| {
            Db::create(MemStorage::default(), &CreateOptions::syncable(hash_key(account), id))
                .unwrap()
        };
        let source = create(1);
        let relay = create(2);
        let target = create(3);
        let mut tx =
            source.begin_write_with_delta(0, crate::store::MAX_REPLICA_DELTA_BYTES).unwrap();
        let key = cipher.record_key(b"volume/test").unwrap();
        let clock = tx.clock();
        let plaintext = vec![42; MAX_PLAINTEXT_BYTES];
        let value = cipher.seal(&key, clock, &plaintext).unwrap();
        tx.put(&key, &value).unwrap();
        let CommittedDelta::Bytes(delta) = tx.commit_with_delta().unwrap() else {
            panic!("maximum record must fit a live delta")
        };
        assert!(!delta.windows(64).any(|bytes| bytes == &plaintext[..64]));
        validate_delta(&delta).unwrap(); // AP shape check needs no client key.
        let mut receive =
            DeltaReceiver::new_with_delta(&relay, Lww, 0, crate::store::MAX_REPLICA_DELTA_BYTES)
                .unwrap();
        receive.process(&delta).unwrap();
        let (_, Some(CommittedDelta::Bytes(forwarded))) = receive.finish_with_delta().unwrap()
        else {
            panic!("relay must capture its actual changes")
        };
        cipher.validate_delta(&forwarded).unwrap();
        for expected in [1, 0] {
            assert_eq!(cipher.apply_delta(&target, &forwarded, 0).unwrap().applied, expected);
        }
        assert_eq!(
            cipher
                .open(&key, clock, &target.begin_read().unwrap().get(&key).unwrap().unwrap())
                .unwrap(),
            plaintext
        );
        assert_eq!(
            source.begin_read().unwrap().root_hash().unwrap(),
            target.begin_read().unwrap().root_hash().unwrap()
        );
        let mut tx =
            source.begin_write_with_delta(0, crate::store::MAX_REPLICA_DELTA_BYTES).unwrap();
        tx.del(&key).unwrap();
        let CommittedDelta::Bytes(deletion) = tx.commit_with_delta().unwrap() else { panic!() };
        assert!(validate_delta(&deletion).is_err());
        assert!(cipher.validate_delta(&deletion).is_err());
    }

    #[test]
    fn forged_clock_or_valid_prefix_with_invalid_tail_never_mutates_destination() {
        let account = Id::from_bytes([1; 16]);
        let cipher = Cipher::new(account, &[7; 16]).unwrap();
        let create = |id| {
            Db::create(MemStorage::default(), &CreateOptions::syncable(hash_key(account), id))
                .unwrap()
        };
        let source = create(1);
        let target = create(2);
        let a = [1; 32];
        let b = [2; 32];
        let mut tx =
            source.begin_write_with_delta(0, crate::store::MAX_REPLICA_DELTA_BYTES).unwrap();
        let value = cipher.seal(&a, tx.clock(), b"valid account record").unwrap();
        tx.put(&a, &value).unwrap();
        // A server can reconstruct a syntactically valid kvdb delta. It cannot
        // authenticate a ciphertext under a different key, even after a valid
        // prefix. Checking only the frame or entries selected by LWW is unsafe.
        tx.put(&b, &value).unwrap();
        let CommittedDelta::Bytes(delta) = tx.commit_with_delta().unwrap() else { panic!() };
        validate_delta(&delta).unwrap();
        assert!(cipher.apply_delta(&target, &delta, 0).is_err());
        assert_eq!(target.begin_read().unwrap().root_hash().unwrap(), None);
        let mut tx =
            source.begin_write_with_delta(0, crate::store::MAX_REPLICA_DELTA_BYTES).unwrap();
        tx.put(&a, &value).unwrap(); // Old ciphertext with a fresh entry clock.
        let CommittedDelta::Bytes(delta) = tx.commit_with_delta().unwrap() else { panic!() };
        assert!(cipher.apply_delta(&target, &delta, 0).is_err());
        assert_eq!(target.begin_read().unwrap().root_hash().unwrap(), None);
    }

    #[test]
    #[ignore = "reference cryptographic timing; run with --release"]
    #[allow(
        clippy::assertions_on_constants,
        reason = "Keep executable checks of the compile-time capacity contract in this regression test."
    )]
    fn benchmark_account_record_cipher() {
        assert!(!cfg!(debug_assertions));
        assert!(std::mem::size_of::<Cipher>() < 1024);
        let cipher = Cipher::new(Id::from_bytes([1; 16]), &[7; 16]).unwrap();
        for size in [512, MAX_PLAINTEXT_BYTES] {
            let plaintext = vec![42; size];
            let mut sealing = Vec::with_capacity(1000);
            let mut opening = Vec::with_capacity(1000);
            for index in 0..1100 {
                let clock = Clock::from_u64(1 + index);
                let start = std::time::Instant::now();
                let key = cipher.record_key(b"volume/00112233445566778899aabbccddeeff").unwrap();
                let value = cipher.seal(&key, clock, &plaintext).unwrap();
                let seal_ns = start.elapsed().as_nanos();
                let start = std::time::Instant::now();
                let opened = cipher.open(&key, clock, &value).unwrap();
                let open_ns = start.elapsed().as_nanos();
                assert_eq!(opened, plaintext);
                if index >= 100 {
                    sealing.push(seal_ns);
                    opening.push(open_ns);
                }
            }
            crate::diagnostic!(
                "account_cipher {}",
                serde_json::json!({
                    "plaintext_bytes":size, "key_state_bytes":std::mem::size_of::<Cipher>(),
                    "seal_and_lookup_ns":sealing, "open_ns":opening,
                })
            );
        }
    }
}
