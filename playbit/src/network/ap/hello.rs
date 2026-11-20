//! Testing HELLO framing. Credentials are supplied explicitly, never discovered
//! from a profile or ordinary bearer token. The local engine has a separate,
//! explicit isolated-profile opt-in that reads a private credentials file.
use super::{Frame, frame_type};

pub const TESTING: u8 = 1;
// Response status, independent of the request flag above.
pub const VALID: u8 = 0;
pub const MISSING: u8 = 1;
pub const INVALID: u8 = 2;
pub const REVOKED: u8 = 3;
pub const MAX_CSR_BYTES: usize = 4096;
const CREDENTIAL_BYTES: usize = 32;
pub const MAX_BODY_BYTES: usize = CREDENTIAL_BYTES + MAX_CSR_BYTES;

const _: () = assert!(MAX_CSR_BYTES > 0);
const _: () = assert!(MAX_BODY_BYTES + 16 <= super::MAX_FRAME_LEN);

#[derive(Clone)]
pub struct TestingCredentials {
    pub key: [u8; 16],
    pub namespace: [u8; 16],
}

impl std::fmt::Debug for TestingCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TestingCredentials { key: [redacted], namespace: [redacted] }")
    }
}

pub struct TestingHello<'a> {
    pub key: [u8; 16],
    pub namespace: [u8; 16],
    pub csr: &'a [u8],
}

impl TestingCredentials {
    pub fn hello(&self, csr: &[u8]) -> Result<Frame, &'static str> {
        if self.namespace == [0; 16] || csr.is_empty() || csr.len() > MAX_CSR_BYTES {
            return Err("invalid testing HELLO");
        }
        let mut body = Vec::with_capacity(CREDENTIAL_BYTES + csr.len());
        body.extend_from_slice(&self.key);
        body.extend_from_slice(&self.namespace);
        body.extend_from_slice(csr);
        assert_eq!(body.len(), CREDENTIAL_BYTES + csr.len(), "HELLO fields match framing");
        Frame::new(frame_type::HELLO, TESTING, [], [], body).map_err(|_| "invalid testing HELLO")
    }
}

// fuzz: ap_codec
pub fn decode(frame: &Frame) -> Result<TestingHello<'_>, &'static str> {
    if frame.frame_type != frame_type::HELLO
        || frame.flags != TESTING
        || !frame.source.is_empty()
        || !frame.destination.is_empty()
        || !((CREDENTIAL_BYTES + 1)..=MAX_BODY_BYTES).contains(&frame.body.len())
    {
        return Err("invalid testing HELLO");
    }
    let namespace =
        frame.body[16..32].try_into().expect("checked fixed-width field has the destination width");
    if namespace == [0; 16] {
        return Err("invalid testing HELLO");
    }
    Ok(TestingHello {
        key: frame.body[..16]
            .try_into()
            .expect("checked fixed-width field has the destination width"),
        namespace,
        csr: &frame.body[32..],
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn exact_testing_wire_and_rejection_boundaries() {
        let credentials = TestingCredentials { key: [0x19; 16], namespace: [0x27; 16] };
        let frame = credentials.hello(&[1, 2, 3]).unwrap();
        let wire = frame.encode().unwrap();
        assert_eq!(&wire[..8], &[51, 0, 0, 0, 0, 1, 0, 0]);
        assert_eq!(&wire[8..16], &[0; 8]);
        assert_eq!(&wire[16..32], &[0x19; 16]);
        assert_eq!(&wire[32..48], &[0x27; 16]);
        assert_eq!(&wire[48..], &[1, 2, 3]);
        assert_eq!(decode(&Frame::decode(&wire).unwrap()).unwrap().csr, &[1, 2, 3]);
        for size in [0, 1, 16, 31, 32, MAX_BODY_BYTES + 1] {
            let mut bad = frame.clone();
            bad.body.resize(size, 1);
            assert!(decode(&bad).is_err());
        }
        for flag in [0, 2, 3, 128, 255] {
            let mut bad = frame.clone();
            bad.flags = flag;
            assert!(decode(&bad).is_err());
        }
        let mut bad = frame.clone();
        bad.source.push(1);
        assert!(decode(&bad).is_err());
        let mut bad = frame.clone();
        bad.destination.push(1);
        assert!(decode(&bad).is_err());
        let mut bad = frame;
        bad.body[16..32].fill(0);
        assert!(decode(&bad).is_err());
        assert!(credentials.hello(&vec![1; MAX_CSR_BYTES]).is_ok());
        assert!(credentials.hello(&vec![1; MAX_CSR_BYTES + 1]).is_err());
        assert!(!format!("{credentials:?}").contains("25"));
    }
}
