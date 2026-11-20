//! Pointer-free authenticated metadata lowered explicitly into wasm32 bytes.
use crate::runtime::syscall::{AuthenticatedFieldRead, FieldProvenance, Principal};
pub(in crate::runtime) fn metadata(record: Option<&AuthenticatedFieldRead>) -> [u8; 160] {
    let mut bytes = [0; 160];
    let Some(record) = record else { return bytes };
    bytes[..16].copy_from_slice(&record.volume_id);
    bytes[144..159].copy_from_slice(&record.entry_id);
    bytes[16..32].copy_from_slice(&record.object_id);
    bytes[32..48].copy_from_slice(&record.type_id);
    bytes[48..64].copy_from_slice(&record.field_id);
    bytes[64..96].copy_from_slice(&record.revision.0);
    bytes[128..132].copy_from_slice(&(1 + u32::from(record.values.is_some())).to_le_bytes());
    let (provenance, principal, device): (u32, _, _) = match record.provenance {
        FieldProvenance::Unproven => return bytes,
        FieldProvenance::ApplicationEdit { principal, device_id } => (1, principal, device_id),
        FieldProvenance::Rekey { principal, device_id } => (2, principal, device_id),
    };
    let (kind, id): (u32, _) = match principal {
        Principal::Human(id) => (1, id),
        Principal::Service(id) => (2, id),
    };
    bytes[96..112].copy_from_slice(&id);
    bytes[112..128].copy_from_slice(&device);
    bytes[132..136].copy_from_slice(&provenance.to_le_bytes());
    bytes[136..140].copy_from_slice(&kind.to_le_bytes());
    bytes
}
