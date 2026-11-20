//! Registration wire fixtures and pre-allocation/range boundaries.
#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;
use crate::network::ap::registration_status;

#[cfg(feature = "local-store")]
#[test]
fn digest_identifies_the_bundled_agreement() {
    assert_eq!(
        ring::digest::digest(&ring::digest::SHA256, TERMS_DOCUMENT.as_bytes()).as_ref(),
        terms_digest()
    );
}
#[test]
fn request_encodes_utf8_byte_lengths() {
    // The fixture is independent of the encoder's concatenation logic.
    let frame = request("person@example.com", "My Mac", "000123", &[1, 2, 3]).unwrap();
    assert_eq!(frame.frame_type, frame_type::REGISTER);
    assert_eq!(frame.flags, 0);
    assert!(frame.source.is_empty() && frame.destination.is_empty());
    assert_eq!(frame.body, b"\x12\0\x06\x06person@example.comMy Mac000123\x01\x02\x03");
    let unicode = request("a@b.co", "É", "", &[]).unwrap();
    assert_eq!(unicode.body[2], 2);
    assert_eq!(&unicode.body[10..], "É".as_bytes());
}
#[test]
fn request_rejects_field_and_total_bounds_before_allocation() {
    // Request-size arithmetic rejects unrepresentable fields and usize overflow.
    assert_eq!(request_length(65536, 0, 0, 0, MAX_BODY_BYTES), Err(Error::FieldTooLong));
    assert_eq!(request_length(0, 256, 0, 0, MAX_BODY_BYTES), Err(Error::FieldTooLong));
    assert_eq!(request_length(0, 0, 256, 0, MAX_BODY_BYTES), Err(Error::FieldTooLong));
    assert!(request_length(65535, 255, 255, usize::MAX, MAX_BODY_BYTES).is_err());
    assert_eq!(request_length(0, 0, 0, MAX_BODY_BYTES - 4, MAX_BODY_BYTES), Ok(MAX_BODY_BYTES));
    assert!(request_length(0, 0, 0, MAX_BODY_BYTES - 3, MAX_BODY_BYTES).is_err());
    assert_eq!(request_length(1, 1, 1, 4089, 4096), Ok(4096));
    assert!(request_length(1, 1, 1, 4090, 4096).is_err());
    assert!(request_length(0, 0, 0, 0, 3).is_err());
    assert!(request("", &"a".repeat(256), "", &[]).is_err());
    let max = request("", "", "", &vec![1; MAX_BODY_BYTES - 4]).unwrap();
    assert_eq!(max.encode().unwrap().len(), MAX_FRAME_LEN);
    assert!(request("", "", "", &vec![1; MAX_BODY_BYTES - 3]).is_err());
}
fn response() -> Frame {
    Frame::new(
        frame_type::REGISTER,
        registration_status::SUCCESS,
        [],
        [],
        b"\x02\x03\x04\0atiidrootinstance",
    )
    .unwrap()
}
#[test]
fn response_borrows_fields_without_adopting_credentials() {
    // Borrow pointers, field bytes and status must agree with the original frame.
    let frame = response();
    let parsed = Response::parse(&frame).unwrap();
    assert_eq!(parsed.status, registration_status::SUCCESS);
    for (field, offset, expected) in [
        (parsed.auth_token, 4, b"at".as_slice()),
        (parsed.instance_id, 6, b"iid".as_slice()),
        (parsed.ca_certificate, 9, b"root".as_slice()),
        (parsed.instance_certificate, 13, b"instance".as_slice()),
    ] {
        assert_eq!(field, expected);
        assert_eq!(field.as_ptr(), frame.body[offset..].as_ptr());
    }
}
#[test]
fn short_declared_ranges_and_invalid_envelopes_are_rejected() {
    // The fixed lengths reject every truncation before the certificate tail.
    let frame = response();
    for length in 0..13 {
        let mut truncated = frame.clone();
        truncated.body.truncate(length);
        assert!(Response::parse(&truncated).is_err());
    }
    for position in 0..4 {
        let mut bad = frame.clone();
        bad.body[position] = 255;
        assert!(Response::parse(&bad).is_err());
    }
    let mut bad = frame.clone();
    bad.frame_type = frame_type::HELLO;
    assert!(matches!(Response::parse(&bad), Err(Error::UnexpectedFrame(0))));
    for source in [false, true] {
        let mut bad = frame.clone();
        if source {
            bad.source.push(1);
        } else {
            bad.destination.push(1);
        }
        assert!(matches!(Response::parse(&bad), Err(Error::InvalidResponse)));
    }
    assert_eq!(response_ends(0, 0, 0, usize::MAX), None);
}
#[test]
fn structural_decode_leaves_status_and_credential_policy_to_caller() {
    // Pending/error replies legitimately contain no credentials; decoding even
    // a SUCCESS status cannot substitute for checking its actual field shapes.
    for status in 0..=u8::MAX {
        let frame = Frame::new(frame_type::REGISTER, status, [], [], [0; 4]).unwrap();
        let parsed = Response::parse(&frame).unwrap();
        assert_eq!(parsed.status, status);
        assert!(parsed.auth_token.is_empty());
        assert!(parsed.instance_id.is_empty());
        assert!(parsed.ca_certificate.is_empty());
        assert!(parsed.instance_certificate.is_empty());
    }
}

#[test]
fn email_codes_preserve_zeroes_and_reject_other_wire_shapes() {
    // Exact bytes survive encoding; separators and the retired length are errors.
    for code in ["000000", "000123", "999999"] {
        assert!(valid_email_code(code.as_bytes()));
        let frame = request("a@b", "Mac", code, &[1]).unwrap();
        assert_eq!(frame.body[3], 6);
        assert_eq!(&frame.body[10..16], code.as_bytes());
    }
    for code in [
        "",
        "12345",
        "1234567",
        "12345678",
        "123–456",
        "123-456",
        "123 456",
        "１２３４５６",
        "١٢٣٤٥٦",
        "abcdef",
        "12345\n",
        " 123456",
        "123456\n",
    ] {
        assert!(!valid_email_code(code.as_bytes()), "{code:?}");
        if !code.is_empty() {
            assert_eq!(request("a@b", "Mac", code, &[1]), Err(Error::InvalidCode));
        }
    }
    assert!(request("a@b", "Mac", "", &[]).is_ok());
}
