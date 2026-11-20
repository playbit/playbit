//! Session links. A link carries one 16-byte seed and nothing else; the seed
//! derives the session signing key (whose public key is the session ID) and
//! the key of an envelope that holds every other credential. The creator seals
//! the envelope once and registers it with the AP, which stores ciphertext it
//! cannot open. Key possession is verified by the platform crypto implementation.
use crate::store::account::Credentials;
use crate::store::catalog::{validate_version, validate_volume_label};
use crate::store::{BuildSelector, Error, Result, Session, Volume};
use serde::{Deserialize, Serialize};

pub const SEED_BYTES: usize = 16;
pub type Seed = [u8; SEED_BYTES];
/// 58^22 exceeds 2^128, so every seed fits a fixed 22-character code.
pub const CODE_CHARS: usize = 22;
pub const MAX_ENVELOPE_BYTES: usize = 4096;
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
const ENVELOPE_OVERHEAD: usize = 29;
const _: () = assert!(MAX_ENVELOPE_BYTES > 1 + 12 + 16);
const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const LOCAL_PREFIX: &str = "session:";
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
const SALT: &[u8] = b"app.playbit.session.v1";
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
const SIGNING_INFO: &[u8] = b"app.playbit.session.signing.v1";
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
const ENVELOPE_INFO: &[u8] = b"app.playbit.session.envelope.v1";
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
const ENVELOPE_DOMAIN: &[u8] = b"app.playbit.session.envelope.v1\0";
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
const PROOF_DOMAIN: &[u8] = b"app.playbit.session.join.v1\0";

/// Everything a session link grants, as the creator sealed it. The seed is
/// the link itself; the rest is the envelope's plaintext.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Invitation {
    pub seed: Seed,
    pub session: Session,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<Volume>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_credentials: Option<Credentials>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared: Option<SharedVolume>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SharedVolume {
    pub volume: Volume,
    pub credentials: Credentials,
}
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    session: Session,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<Volume>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_credentials: Option<Credentials>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shared: Option<SharedVolume>,
}

pub fn session_code(seed: &Seed) -> String {
    let mut value = u128::from_be_bytes(*seed);
    let mut code = [b'1'; CODE_CHARS];
    for slot in code.iter_mut().rev() {
        *slot = ALPHABET[(value % 58) as usize];
        value /= 58;
    }
    assert_eq!(value, 0, "fixed-width base58 code holds the complete seed");
    code.iter().map(|&byte| byte as char).collect()
}
pub fn session_link(seed: &Seed, channel: crate::release_channel::ReleaseChannel) -> String {
    format!("{}/s#{}", channel.website(), session_code(seed))
}
pub fn session_uri(seed: &Seed, channel: crate::release_channel::ReleaseChannel) -> String {
    format!("playbit:session:{}:{}", channel.name(), session_code(seed))
}
// fuzz: store_contract
pub fn parse_session_code(code: &str) -> Result<Seed> {
    if code.len() != CODE_CHARS {
        return Err(Error::InvalidInput("session code must be 22 base58 characters"));
    }
    let mut value: u128 = 0;
    for byte in code.bytes() {
        let digit = ALPHABET
            .iter()
            .position(|&letter| letter == byte)
            .ok_or(Error::InvalidInput("session code must be 22 base58 characters"))?;
        value = value
            .checked_mul(58)
            .and_then(|value| value.checked_add(digit as u128))
            .ok_or(Error::InvalidInput("invalid session code"))?;
    }
    Ok(value.to_be_bytes())
}
/// The AP carries envelopes as unpadded base64url strings.
pub fn envelope_text(envelope: &[u8]) -> String {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    URL_SAFE_NO_PAD.encode(envelope)
}
// fuzz: store_contract
pub fn parse_envelope_text(text: &str) -> Result<Vec<u8>> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    // Exact decoded length for unpadded base64; divide before multiplying.
    if text.len() / 4 * 3 + text.len() % 4 * 3 / 4 > MAX_ENVELOPE_BYTES {
        return Err(Error::InvalidInput("invalid invitation envelope"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| Error::InvalidInput("invalid invitation envelope"))?;
    assert!(bytes.len() <= MAX_ENVELOPE_BYTES, "text admission bounds decoded allocation");
    Ok(bytes)
}
/// A qualified web/native link must match the selected channel. Local `session:`,
/// `s#` and bare codes explicitly use the caller's selected environment.
// fuzz: store_contract
pub fn parse_session_link(
    text: &str,
    selected: crate::release_channel::ReleaseChannel,
) -> Result<Seed> {
    if text.len() > 256 {
        return Err(Error::InvalidInput("invalid session link"));
    }
    use crate::release_channel::ReleaseChannel;
    for channel in ReleaseChannel::ALL {
        let code = text
            .strip_prefix(channel.website())
            .and_then(|rest| rest.strip_prefix("/s#"))
            .or_else(|| {
                text.strip_prefix("playbit:session:")
                    .and_then(|rest| rest.strip_prefix(channel.name()))
                    .and_then(|rest| rest.strip_prefix(':'))
            });
        if let Some(code) = code {
            if channel != selected {
                return Err(Error::InvalidInput(match channel {
                    ReleaseChannel::Public if selected.is_personal() => {
                        "This invitation belongs to Public. Open a separate Playbit instance in Public to join it"
                    }
                    ReleaseChannel::Testing if selected.is_personal() => {
                        "This invitation belongs to shared Testing. Open a separate Playbit instance in shared Testing to join it"
                    }
                    ReleaseChannel::Public => {
                        "This invitation belongs to Public. Choose Public in the account or sign-in Environment menu, then reopen the invitation"
                    }
                    ReleaseChannel::Testing => {
                        "This invitation belongs to Testing. Choose Testing in the account or sign-in Environment menu, then reopen the invitation"
                    }
                    #[cfg(test)]
                    _ => "This invitation belongs to another personal Testing environment",
                }));
            }
            return parse_session_code(code);
        }
    }
    let code = text.strip_prefix("s#").or_else(|| text.strip_prefix(LOCAL_PREFIX)).unwrap_or(text);
    parse_session_code(code)
}

impl Invitation {
    fn replica(&self) -> Result<(&Volume, &Credentials)> {
        match (&self.root, &self.root_credentials, &self.shared, self.session.shared_volume) {
            (Some(volume), Some(credentials), None, None) => Ok((volume, credentials)),
            (None, None, Some(shared), Some(_)) => Ok((&shared.volume, &shared.credentials)),
            _ => Err(Error::InvalidInput("invitation capability shape differs")),
        }
    }
    pub fn replica_volume(&self) -> Result<&Volume> {
        self.replica().map(|(volume, _)| volume)
    }
    pub fn replica_credentials(&self) -> Result<&Credentials> {
        self.replica().map(|(_, credentials)| credentials)
    }
    /// Structural checks shared by every decoder; key possession is separate.
    // fuzz: store_contract
    pub(crate) fn validate(&self) -> Result<()> {
        let (volume, credentials) = self.replica()?;
        validate_volume_label(&volume.name)?;
        let session = &self.session;
        session.app.validate().map_err(Error::InvalidInput)?;
        session.validate_shared_volume()?;
        if session.shared_volume.is_none()
            && matches!(session.selector, BuildSelector::Exact { .. })
            && session.app.publisher != session.creator
        {
            return Err(Error::InvalidInput("direct app publisher differs from session creator"));
        }
        validate_version(&session.id)?;
        validate_version(&session.resolved_build_id)?;
        if session.replica_volume() != volume.id || credentials.database_key == credentials.blob_key
        {
            return Err(Error::InvalidInput("invitation identity or credential mismatch"));
        }
        if session.shared_volume.is_some()
            && (volume.owner != session.creator
                || volume.personal
                || !volume.share_key_with_service)
        {
            return Err(Error::AccessDenied("Shared invitation authority differs"));
        }
        match &session.selector {
            BuildSelector::Exact { build_id } if build_id != &session.resolved_build_id => {
                Err(Error::InvalidInput("invitation version mismatch"))
            }
            BuildSelector::App { app } if app != &session.app => {
                Err(Error::InvalidInput("invalid invitation app identity"))
            }
            _ => Ok(()),
        }
    }
    pub fn link(&self, channel: crate::release_channel::ReleaseChannel) -> String {
        session_link(&self.seed, channel)
    }
}

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub(crate) struct SessionKeys {
    signing: ring::signature::Ed25519KeyPair,
    envelope: ring::aead::LessSafeKey,
}
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
impl SessionKeys {
    pub(crate) fn derive(seed: &Seed) -> Result<Self> {
        use ring::{aead, hkdf, signature::Ed25519KeyPair};
        let root = hkdf::Salt::new(hkdf::HKDF_SHA256, SALT).extract(seed);
        let derive = |info: &[u8]| -> Result<[u8; 32]> {
            let mut key = [0; 32];
            root.expand(&[info], hkdf::HKDF_SHA256)
                .map_err(|_| Error::Internal("invitation key derivation expansion failed"))?
                .fill(&mut key)
                .map_err(|_| Error::Internal("invitation key derivation output failed"))?;
            Ok(key)
        };
        Ok(Self {
            signing: Ed25519KeyPair::from_seed_unchecked(&derive(SIGNING_INFO)?)
                .map_err(|_| Error::Internal("could not construct the invitation signing key"))?,
            envelope: aead::LessSafeKey::new(
                aead::UnboundKey::new(&aead::AES_256_GCM, &derive(ENVELOPE_INFO)?).map_err(
                    |_| Error::Internal("could not construct the invitation encryption key"),
                )?,
            ),
        })
    }
    pub(crate) fn public_key(&self) -> &[u8] {
        use ring::signature::KeyPair;
        self.signing.public_key().as_ref()
    }
    pub(crate) fn session_id(&self) -> String {
        crate::identifier::encode(self.public_key())
    }
    /// Domain-separated possession proof over the joining account's raw ID.
    pub(crate) fn proof(&self, account: crate::store::Id) -> Vec<u8> {
        let mut message = PROOF_DOMAIN.to_vec();
        message.extend_from_slice(account.as_bytes());
        self.signing.sign(&message).as_ref().to_vec()
    }
    fn aad(&self) -> Vec<u8> {
        let mut bytes = ENVELOPE_DOMAIN.to_vec();
        bytes.extend_from_slice(self.public_key());
        bytes
    }
}
/// The session ID a seed identifies, without touching any store.
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub fn session_id(seed: &Seed) -> Result<String> {
    Ok(SessionKeys::derive(seed)?.session_id())
}

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
impl Invitation {
    /// Key possession: the seed must derive the session's own public key.
    pub(crate) fn verify(&self) -> Result<SessionKeys> {
        self.validate()?;
        let keys = SessionKeys::derive(&self.seed)?;
        if keys.session_id() != self.session.id {
            return Err(Error::InvalidInput("invitation identity or credential mismatch"));
        }
        Ok(keys)
    }
    /// `1 ‖ nonce ‖ ciphertext ‖ tag`, bound to the session's public key.
    pub(crate) fn seal(&self) -> Result<Vec<u8>> {
        self.seal_with_nonce(crate::store::random_array()?)
    }
    fn seal_with_nonce(&self, nonce: [u8; 12]) -> Result<Vec<u8>> {
        use ring::aead;
        let keys = self.verify()?;
        let plaintext = serde_json::to_vec(&Body {
            session: self.session.clone(),
            root: self.root.clone(),
            root_credentials: self.root_credentials.clone(),
            shared: self.shared.clone(),
        })
        .map_err(|_| Error::Internal("could not encode an invitation"))?;
        if plaintext.len() > MAX_ENVELOPE_BYTES - ENVELOPE_OVERHEAD {
            return Err(Error::LimitExceeded);
        }
        let mut value = Vec::with_capacity(ENVELOPE_OVERHEAD + plaintext.len());
        value.push(1);
        value.extend_from_slice(&nonce);
        value.extend_from_slice(&plaintext);
        let tag = keys
            .envelope
            .seal_in_place_separate_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(keys.aad()),
                &mut value[13..],
            )
            .map_err(|_| Error::System("invitation encryption failed".into()))?;
        value.extend_from_slice(tag.as_ref());
        assert_eq!(value.len(), plaintext.len() + ENVELOPE_OVERHEAD, "envelope wire overhead");
        assert!(value.len() <= MAX_ENVELOPE_BYTES, "admitted plaintext fits the envelope");
        Ok(value)
    }
    // fuzz: store_contract
    pub fn open(seed: &Seed, envelope: &[u8]) -> Result<Self> {
        use ring::aead;
        if !(ENVELOPE_OVERHEAD + 1..=MAX_ENVELOPE_BYTES).contains(&envelope.len())
            || envelope[0] != 1
        {
            return Err(Error::InvalidInput("invalid invitation envelope"));
        }
        let keys = SessionKeys::derive(seed)?;
        let mut plaintext = envelope[13..].to_vec();
        let length = keys
            .envelope
            .open_in_place(
                aead::Nonce::assume_unique_for_key(
                    envelope[1..13]
                        .try_into()
                        .expect("checked fixed-width field has the destination width"),
                ),
                aead::Aad::from(keys.aad()),
                &mut plaintext,
            )
            .map_err(|_| Error::AccessDenied("invitation envelope authentication failed"))?
            .len();
        assert_eq!(length + ENVELOPE_OVERHEAD, envelope.len(), "authenticated plaintext length");
        plaintext.truncate(length);
        let body: Body = serde_json::from_slice(&plaintext).map_err(Error::decode::<Body>)?;
        let invitation = Self {
            seed: *seed,
            session: body.session,
            root: body.root,
            root_credentials: body.root_credentials,
            shared: body.shared,
        };
        invitation.verify()?;
        Ok(invitation)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn qualified_links_and_native_uris_require_their_selected_authority() {
        use crate::release_channel::ReleaseChannel;
        let seed = [7; SEED_BYTES];
        for channel in ReleaseChannel::ALL {
            for text in [session_link(&seed, channel), session_uri(&seed, channel)] {
                assert_eq!(parse_session_link(&text, channel).unwrap(), seed);
                for other in ReleaseChannel::ALL.into_iter().filter(|other| *other != channel) {
                    assert!(
                        parse_session_link(&text, other)
                            .unwrap_err()
                            .to_string()
                            .contains("belongs to")
                    );
                }
            }
            assert_eq!(parse_session_link(&session_code(&seed), channel).unwrap(), seed);
            // OS-delivered web links must retain their exact authority and fragment shape.
            let origin = channel.website();
            let code = session_code(&seed);
            for text in [
                format!("{}/s#{code}", origin.replacen("https:", "http:", 1)),
                format!("{origin}.example.org/s#{code}"),
                format!("{origin}:443/s#{code}"),
                format!("{origin}/s/#{code}"),
                format!("{origin}/s/other#{code}"),
                format!("{origin}/s?query=1#{code}"),
                format!("{origin}/s#{code}#other"),
                format!("{origin}/s#{}", "0".repeat(CODE_CHARS)),
                format!("{origin}/s"),
            ] {
                assert!(parse_session_link(&text, channel).is_err());
            }
        }
    }

    fn unhex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// The vector playbit/platform/web/src/invitation.test.mjs checks with WebCrypto.
    #[test]
    fn native_matches_web_crypto_vector() {
        let vector: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/session-link-v1.json"))
                .unwrap();
        let seed: Seed = unhex(vector["seed"].as_str().unwrap()).try_into().unwrap();
        let code = vector["code"].as_str().unwrap();
        assert_eq!(session_code(&seed), code);
        assert_eq!(
            parse_session_link(
                &format!("https://playbit.app/s#{code}"),
                crate::release_channel::ReleaseChannel::Public
            )
            .unwrap(),
            seed
        );
        let keys = SessionKeys::derive(&seed).unwrap();
        assert_eq!(keys.session_id(), vector["session_id"].as_str().unwrap());
        let account = crate::store::Id::parse(vector["account"].as_str().unwrap()).unwrap();
        assert_eq!(keys.proof(account), unhex(vector["proof"].as_str().unwrap()));
        let body: Body = serde_json::from_str(vector["plaintext"].as_str().unwrap()).unwrap();
        let invitation = Invitation {
            seed,
            session: body.session,
            root: body.root,
            root_credentials: body.root_credentials,
            shared: body.shared,
        };
        let nonce = unhex(vector["nonce"].as_str().unwrap()).try_into().unwrap();
        let envelope = parse_envelope_text(vector["envelope"].as_str().unwrap()).unwrap();
        assert_eq!(invitation.seal_with_nonce(nonce).unwrap(), envelope);
        assert_eq!(Invitation::open(&seed, &envelope).unwrap(), invitation);
        let mut invalid = invitation.clone();
        invalid.root.as_mut().unwrap().name = "x".repeat(257);
        assert!(invalid.seal_with_nonce(nonce).is_err());
        invalid = invitation.clone();
        invalid.root.as_mut().unwrap().name = " ".into();
        assert!(invalid.seal_with_nonce(nonce).is_err());
        assert_ne!(invitation.seal().unwrap(), envelope);
        assert!(Invitation::open(&[9; SEED_BYTES], &envelope).is_err());
        for index in 0..envelope.len() {
            let mut corrupt = envelope.clone();
            corrupt[index] ^= 1;
            assert!(Invitation::open(&seed, &corrupt).is_err());
            assert!(Invitation::open(&seed, &envelope[..index]).is_err());
        }
    }

    #[test]
    fn envelope_text_enforces_exact_decoded_limit() {
        // Check exact decoded boundaries and reject noncanonical base64 without padding.
        for len in [0, 1, MAX_ENVELOPE_BYTES - 1, MAX_ENVELOPE_BYTES] {
            let bytes = vec![0xff; len];
            assert_eq!(parse_envelope_text(&envelope_text(&bytes)).unwrap(), bytes);
        }
        for len in MAX_ENVELOPE_BYTES + 1..=MAX_ENVELOPE_BYTES + 3 {
            assert!(parse_envelope_text(&envelope_text(&vec![0xff; len])).is_err());
        }
        for invalid in ["A", "AA=", "+/", "AB", "é"] {
            assert!(parse_envelope_text(invalid).is_err());
        }
    }

    #[test]
    fn shared_invitation_retains_one_creator_backing_and_rejects_mixed_capabilities() {
        use crate::package::AutomaticVolumeRole;
        let vector: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/session-link-v1.json"))
                .unwrap();
        let seed = unhex(vector["seed"].as_str().unwrap()).try_into().unwrap();
        let envelope = parse_envelope_text(vector["envelope"].as_str().unwrap()).unwrap();
        let legacy = Invitation::open(&seed, &envelope).unwrap();
        let mut invitation = legacy.clone();
        let session = &mut invitation.session;
        session.root_volume =
            session.app.volume_id(AutomaticVolumeRole::Root, None, None, None).unwrap();
        session.shared_volume = Some(
            session
                .app
                .volume_id(
                    AutomaticVolumeRole::Session,
                    Some(crate::identifier::decode(&session.id).unwrap()),
                    None,
                    None,
                )
                .unwrap(),
        );
        invitation.shared = Some(SharedVolume {
            volume: Volume {
                id: session.shared_volume.unwrap(),
                name: "Shared".into(),
                owner: session.creator,
                share_key_with_service: true,
                personal: false,
            },
            credentials: invitation.root_credentials.take().unwrap(),
        });
        invitation.root = None;
        let sealed = invitation.seal().unwrap();
        assert_eq!(Invitation::open(&seed, &sealed).unwrap(), invitation);
        let json = serde_json::to_value(&invitation).unwrap();
        assert!(json.get("root").is_none());
        assert!(json.get("root_credentials").is_none());

        let mut invalid = invitation.clone();
        invalid.root = legacy.root;
        invalid.root_credentials = legacy.root_credentials;
        assert!(invalid.seal().is_err(), "mixed capability arms cannot grant more authority");
        invalid = invitation.clone();
        invalid.shared.as_mut().unwrap().volume.owner = crate::store::Id([0; 16]);
        assert!(invalid.seal().is_err());
        invalid = invitation.clone();
        invalid.session.shared_volume = Some(crate::store::Id([0; 16]));
        assert!(invalid.seal().is_err());
        invalid = invitation.clone();
        invalid.session.root_volume = invalid.session.shared_volume.unwrap();
        assert!(invalid.seal().is_err(), "Shared never aliases World");
        invalid = invitation;
        invalid.shared.as_mut().unwrap().volume.share_key_with_service = false;
        assert!(invalid.seal().is_err());
    }

    #[test]
    fn codes_are_fixed_width_round_trip_and_reject_other_text() {
        for seed in [[0; 16], [0xff; 16], [7; 16], *b"0123456789abcdef"] {
            let code = session_code(&seed);
            assert_eq!(code.len(), CODE_CHARS);
            assert!(code.bytes().all(|byte| ALPHABET.contains(&byte)));
            assert_eq!(parse_session_code(&code).unwrap(), seed);
            assert_eq!(
                parse_session_link(
                    &session_link(&seed, crate::release_channel::ReleaseChannel::Public),
                    crate::release_channel::ReleaseChannel::Public
                )
                .unwrap(),
                seed
            );
            assert_eq!(
                parse_session_link(
                    &format!("s#{code}"),
                    crate::release_channel::ReleaseChannel::Public
                )
                .unwrap(),
                seed
            );
            assert_eq!(
                parse_session_link(
                    &session_uri(&seed, crate::release_channel::ReleaseChannel::Public),
                    crate::release_channel::ReleaseChannel::Public
                )
                .unwrap(),
                seed
            );
            assert_eq!(
                parse_session_link(
                    &format!("session:{code}"),
                    crate::release_channel::ReleaseChannel::Public
                )
                .unwrap(),
                seed
            );
            assert_eq!(
                parse_session_link(&code, crate::release_channel::ReleaseChannel::Public).unwrap(),
                seed
            );
        }
        assert_eq!(session_code(&[0; 16]), "1".repeat(CODE_CHARS));
        for invalid in [
            "",
            "0".repeat(22).as_str(),
            "1".repeat(21).as_str(),
            "z".repeat(22).as_str(),
            "https://example.org/s#1111111111111111111111",
            "x".repeat(300).as_str(),
        ] {
            assert!(
                parse_session_link(invalid, crate::release_channel::ReleaseChannel::Public)
                    .is_err(),
                "{invalid}"
            );
        }
    }
}
