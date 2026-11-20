#![allow(
    clippy::unwrap_used,
    reason = "Tests exercise fixed format vectors and refusal boundaries."
)]
use super::*;
use crate::store::Volume;

fn volume() -> Volume {
    Volume {
        id: Id::from_bytes([0; 16]),
        name: "Notes".into(),
        owner: Id::from_bytes([0; 16]),
        share_key_with_service: false,
        personal: true,
    }
}
fn extension(bytes: &mut Vec<u8>, tag: u16, body: &[u8]) {
    bytes.extend_from_slice(&tag.to_le_bytes());
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(body);
}

#[test]
fn fixed_entry_acl_vectors_keep_their_released_meanings() {
    // UI preference retention adds no fields, variants or changed bytes to the active ACL.
    use crate::store::object_contract::{Acl, EntryAcl};
    let inherited = b"PBR\x01\x0a\x01\x00\x08";
    let replaced =
        b"PBR\x01\x0a\x02\x00\x08\x01\xc0\x08\x00\x00\x00\x08\x01\xc0\x01\x00\x00\x00\x07";
    for (bytes, expected) in [
        (inherited.as_slice(), EntryAcl::Inherit),
        (replaced.as_slice(), EntryAcl::Replace(Acl::default())),
    ] {
        assert_eq!(decode::<EntryAcl>(bytes).unwrap(), expected);
        assert_eq!(encode(&expected).unwrap(), bytes);
    }
}

#[test]
fn fixed_baseline_volume_vector_is_readable_without_regenerating_old_bytes() {
    // This independent literal freezes framing, fixed IDs, flag bits and tag order.
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../../../tests/fixtures/client-record-v1.json"))
            .unwrap();
    let vector = fixture["volume_hex_parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| part.as_str().unwrap())
        .collect::<String>();
    let bytes: Vec<u8> = vector
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    assert_eq!(decode::<Volume>(&bytes).unwrap(), volume());
    assert_eq!(encode(&volume()).unwrap(), bytes);
}

#[test]
fn older_writer_preserves_unknown_optional_body_and_refuses_required_semantics() {
    // Unknown wire kinds are opaque only inside optional fields; name edits keep them.
    let mut bytes = encode(&volume()).unwrap();
    extension(&mut bytes, 99, &[255, 7, 0, 9]);
    let mut renamed = decode::<Volume>(&bytes).unwrap();
    renamed.name = "Changed".into();
    let changed = rewrite(&bytes, &renamed).unwrap();
    assert_eq!(decode::<Volume>(&changed).unwrap(), renamed);
    assert_eq!(&changed[changed.len() - 10..], &bytes[bytes.len() - 10..]);
    for (flags, reads) in [(READ_REQUIRED, false), (WRITE_REQUIRED, true), (REQUIRED, false)] {
        let mut bytes = encode(&volume()).unwrap();
        extension(&mut bytes, 99 | flags, &[255, 7]);
        assert_eq!(read::<Volume>(&bytes).is_ok(), reads);
        assert!(decode::<Volume>(&bytes).is_err());
        assert!(rewrite(&bytes, &renamed).is_err());
    }
}

#[derive(Debug, PartialEq)]
struct Inner {
    name: String,
}
fields!(Inner { 1 => name });
#[derive(Debug, PartialEq)]
struct Outer {
    version: u64,
    child: Inner,
}
fields!(Outer { 1 => version, 2 => child });

#[test]
fn nested_optional_fields_survive_known_struct_edits() {
    // Construct a newer child's opaque tag without teaching the older typed codec it.
    let mut child = Vec::new();
    Inner { name: "Old".into() }.write(&mut child).unwrap();
    extension(&mut child, 70, &[200, 1, 2]);
    let mut bytes = Vec::from(MAGIC);
    bytes.push(STRUCT);
    put(&mut bytes, 1, &1u64).unwrap();
    put_raw(&mut bytes, 2, &child).unwrap();
    let next = Outer { version: 2, child: Inner { name: "New".into() } };
    let changed = rewrite(&bytes, &next).unwrap();
    assert_eq!(decode::<Outer>(&changed).unwrap(), next);
    let retained =
        View::record(&changed).unwrap().fields(Mode::Write, &[1, 2]).unwrap().get(2).unwrap();
    assert_eq!(&retained.0[retained.0.len() - 9..], &child[child.len() - 9..]);
}

#[test]
fn changed_sequences_with_unmapped_nested_extensions_refuse() {
    // Reordering must not attach an opaque extension to an unrelated sequence item.
    let mut item = Vec::new();
    Inner { name: "First".into() }.write(&mut item).unwrap();
    extension(&mut item, 50, &[111, 2]);
    let mut bytes = Vec::from(MAGIC);
    bytes.push(SEQUENCE);
    bytes.extend_from_slice(&(item.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&item);
    assert!(rewrite(&bytes, &vec![Inner { name: "Other".into() }]).is_err());
    assert_eq!(rewrite(&bytes, &vec![Inner { name: "First".into() }]).unwrap(), bytes);
}

#[test]
fn additive_optional_field_has_a_defined_absent_default_and_survives_an_older_writer() {
    // A future reader defines tag 6's missing value as zero. The older Volume
    // reader can preserve it while editing an existing tag it understands.
    fn future_counter(bytes: &[u8]) -> Result<u64> {
        let fields = View::record(bytes)?.fields(Mode::Read, &[1, 2, 3, 4, 5])?;
        fields
            .optional(6)?
            .map(|value| u64::read(value, Mode::Read))
            .transpose()
            .map(|value| value.unwrap_or(0))
    }
    let mut bytes = encode(&volume()).unwrap();
    assert_eq!(future_counter(&bytes).unwrap(), 0);
    let mut counter = Vec::new();
    7u64.write(&mut counter).unwrap();
    extension(&mut bytes, 6, &counter);
    assert_eq!(future_counter(&bytes).unwrap(), 7);
    let mut old = decode::<Volume>(&bytes).unwrap();
    old.name = "Edited".into();
    let changed = rewrite(&bytes, &old).unwrap();
    assert_eq!(future_counter(&changed).unwrap(), 7);
}

#[test]
fn malformed_frames_and_excess_opaque_extensions_are_rejected() {
    // Truncate every byte boundary and corrupt duplicate tags, lengths and flags.
    let bytes = encode(&volume()).unwrap();
    for end in 0..bytes.len() {
        assert!(decode::<Volume>(&bytes[..end]).is_err());
    }
    let mut duplicate = bytes.clone();
    extension(&mut duplicate, 5 | REQUIRED, &[BOOLEAN, 1]);
    assert!(decode::<Volume>(&duplicate).is_err());
    let mut flags = bytes.clone();
    flags[6] = 0;
    assert!(decode::<Volume>(&flags).is_err());
    let mut length = bytes.clone();
    length[7..11].fill(255);
    assert!(decode::<Volume>(&length).is_err());
    let mut too_large = bytes.clone();
    extension(&mut too_large, 99, &vec![255; MAX_EXTENSION_BYTES]);
    assert!(decode::<Volume>(&too_large).is_err());
    let mut invalid_boolean = bytes;
    *invalid_boolean.last_mut().unwrap() = 2;
    assert!(decode::<Volume>(&invalid_boolean).is_err());
}
