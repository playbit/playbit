//! Ephemeral session routing. Destinations are raw session keys followed by UTF-8 names.
//! Transport authentication identifies the account; possession proofs grant channel access.
use super::Frame;
#[cfg(test)]
use super::frame_type;

pub use crate::session_channel::*;
/// The first routing frame carries a possession proof and optional app identity.
/// The gateway replaces the source with authenticated account/device information.
pub fn first_frame(
    kind: u8,
    destination: Vec<u8>,
    proof: &[u8],
    instance: Option<([u8; 16], [u8; 32])>,
    payload: &[u8],
) -> Result<Frame, super::Error> {
    let source = instance.map_or_else(Vec::new, |(id, build)| [id.as_slice(), &build].concat());
    Frame::new(kind, SESSION, source, destination, [proof, payload].concat())
}
pub fn acknowledgement(frame: &Frame, kind: u8, destination: &[u8]) -> Result<(), &'static str> {
    if frame.frame_type != kind
        || frame.destination != destination
        || !frame.source.is_empty()
        || !frame.body.is_empty()
    {
        return Err("invalid channel acknowledgement");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn destinations_preserve_exact_name_bytes_and_frame_boundaries() {
        let session = [42; 32];
        for name in ["a/b", "\0", "é", "e\u{301}"] {
            let d = destination(&session, name).expect("valid name");
            assert_eq!(&d[..32], &session);
            assert_eq!(&d[32..], name.as_bytes());
            assert_eq!(validate_destination(&d), Ok(&session));
        }
        assert!(destination(&session, &"x".repeat(223)).is_ok());
        assert!(destination(&session, &"x".repeat(224)).is_err());
        assert!(destination(&session, "").is_err());
        assert!(validate_destination(&[session.as_slice(), &[255]].concat()).is_err());
        assert!(
            acknowledgement(
                &Frame::new(frame_type::PUBLISH, 0, [], session, []).expect("frame"),
                frame_type::PUBLISH,
                &session
            )
            .is_ok()
        );
    }
}
