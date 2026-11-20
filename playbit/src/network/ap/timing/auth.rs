//! Mandatory diagnostic envelope for binary authentication replies. Credential
//! bodies are wrapped only at send time, never in durable enrollment records.
use super::{MAX_ENCODED_BYTES, Timings};
use crate::network::ap::{Error, Frame, Result, frame_type};
pub const MAX_REPLY_BYTES: usize = 4096;
const PREFIX_BYTES: usize = 2;

fn payload_start(timing_bytes: u16, total: usize) -> Option<usize> {
    let timing_bytes = usize::from(timing_bytes);
    let start = PREFIX_BYTES + timing_bytes;
    ((2..=MAX_ENCODED_BYTES).contains(&timing_bytes) && total <= MAX_REPLY_BYTES && start <= total)
        .then_some(start)
}
fn validate(frame: &Frame) -> Result<()> {
    if !matches!(
        frame.frame_type,
        frame_type::HELLO | frame_type::REGISTER | frame_type::KEY_CHALLENGE
    ) || !frame.source.is_empty()
        || !frame.destination.is_empty()
    {
        return Err(Error::InvalidHeader);
    }
    Ok(())
}

/// Encode a fresh projection beside the original authentication payload. No
/// failure mutates a durable credential response or admits a larger frame.
pub fn encode(mut frame: Frame, timings: &Timings) -> Result<Frame> {
    validate(&frame)?;
    let timing = serde_json::to_vec(timings).map_err(|_| Error::InvalidTiming)?;
    let length = u16::try_from(timing.len()).map_err(|_| Error::FrameTooLarge)?;
    let total =
        frame.body.len().checked_add(PREFIX_BYTES + timing.len()).ok_or(Error::FrameTooLarge)?;
    payload_start(length, total).ok_or(Error::FrameTooLarge)?;
    let mut body = Vec::with_capacity(total);
    body.extend_from_slice(&length.to_le_bytes());
    body.extend_from_slice(&timing);
    body.extend_from_slice(&frame.body);
    frame.body = body;
    Ok(frame)
}

/// Structural decoding does not authenticate. Callers still validate the
/// returned frame's status and credentials before adopting any identity.
// fuzz: ap_codec
pub fn decode(mut frame: Frame) -> Result<(Frame, Timings)> {
    validate(&frame)?;
    let prefix: [u8; PREFIX_BYTES] = frame
        .body
        .get(..PREFIX_BYTES)
        .ok_or(Error::InvalidTiming)?
        .try_into()
        .map_err(|_| Error::InvalidTiming)?;
    let start =
        payload_start(u16::from_le_bytes(prefix), frame.body.len()).ok_or(Error::InvalidTiming)?;
    let timings = serde_json::from_slice(&frame.body[PREFIX_BYTES..start])
        .map_err(|_| Error::InvalidTiming)?;
    frame.body.drain(..start);
    Ok((frame, timings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::ap::timing::{Stage, Timing};
    #[test]
    fn exact_authentication_budget_preserves_payload_and_rejects_oversize() {
        let mut timings = Timings::default();
        for stage in Stage::ALL {
            timings.push(Timing { stage, duration_ns: u64::MAX }).expect("distinct stage");
        }
        let overhead = PREFIX_BYTES + serde_json::to_vec(&timings).expect("timings").len();
        let payload = vec![7; MAX_REPLY_BYTES - overhead];
        let plain = Frame::new(frame_type::REGISTER, 1, [], [], payload).expect("bounded payload");
        let encoded = encode(plain.clone(), &timings).expect("exact admitted reply");
        assert_eq!(encoded.body.len(), MAX_REPLY_BYTES);
        let (decoded, observed) = decode(encoded).expect("round trip");
        assert_eq!(decoded, plain);
        assert_eq!(observed, timings);
        let mut excessive = plain;
        excessive.body.push(8);
        assert_eq!(encode(excessive, &timings), Err(Error::FrameTooLarge));
    }
    #[test]
    fn unknown_optional_stage_preserves_success_rejection_and_credential_bytes() {
        let timing = br#"[{"stage":"ap.future_storage","duration_ns":"18446744073709551615"}]"#;
        for (kind, flags) in
            [(frame_type::HELLO, 0), (frame_type::REGISTER, 1), (frame_type::KEY_CHALLENGE, 2)]
        {
            let mut body = Vec::new();
            body.extend_from_slice(&(timing.len() as u16).to_le_bytes());
            body.extend_from_slice(timing);
            body.extend_from_slice(&[7, 8, 9]);
            let frame = Frame::new(kind, flags, [], [], body).expect("bounded wire fixture");
            let (decoded, observed) = decode(frame).expect("unknown optional annotation");
            assert_eq!(decoded.flags, flags);
            assert_eq!(decoded.body, [7, 8, 9]);
            assert!(observed.entries().is_empty());
        }
    }
    #[test]
    fn malformed_metadata_is_rejected_before_credential_adoption() {
        for body in [
            vec![],
            vec![0],
            vec![0, 0],
            vec![1, 0, b'0'],
            vec![255, 255],
            vec![2, 0, b'[', b'0'],
            vec![2, 0, b'{', b'}'],
            vec![0; MAX_REPLY_BYTES + 1],
        ] {
            let frame = Frame::new(frame_type::HELLO, 0, [], [], body).expect("wire frame");
            assert!(decode(frame).is_err());
        }
        let frame = Frame::new(frame_type::HELLO, 2, [], [], []).expect("rejected reply");
        let (decoded, timings) =
            decode(encode(frame.clone(), &Timings::default()).expect("envelope"))
                .expect("rejection envelope");
        assert_eq!(decoded, frame);
        assert!(timings.entries().is_empty());
    }
}
