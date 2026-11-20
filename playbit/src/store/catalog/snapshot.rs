//! Snapshot cuts qualify a source across backings; they grant no authority or
//! hosted durability. Source callers capture them under their volume guard.
use super::{DurableRevision, Error, Id, ReplicaRoot, Result};
use crate::store::volume_index::namespace::receipt::Receipt as NamespaceReceipt;
use serde::{Deserialize, Serialize};

/// A recreated backend fence gets another generation to prevent epoch reuse.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotToken {
    pub generation: Id,
    pub epoch: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "ReceiptWire")]
pub struct ReplicaReceipt {
    #[serde(flatten)]
    pub durable: DurableRevision,
    pub source: SnapshotToken,
    pub namespace: NamespaceReceipt,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptWire {
    revision: u64,
    policy_revision: u64,
    root: ReplicaRoot,
    source: SnapshotToken,
    namespace: NamespaceReceipt,
}
impl TryFrom<ReceiptWire> for ReplicaReceipt {
    type Error = Error;
    fn try_from(wire: ReceiptWire) -> Result<Self> {
        SnapshotCut::new(wire.source, wire.policy_revision)?;
        wire.namespace.validate()?;
        Ok(Self {
            durable: DurableRevision {
                revision: wire.revision,
                policy_revision: wire.policy_revision,
                root: wire.root,
            },
            source: wire.source,
            namespace: wire.namespace,
        })
    }
}
#[derive(Clone, Copy, Debug)]
pub struct SnapshotCut {
    source: SnapshotToken,
    policy_revision: u64,
}
impl SnapshotCut {
    pub fn new(source: SnapshotToken, policy_revision: u64) -> Result<Self> {
        if source.generation == Id::from_bytes([0; 16]) || policy_revision == 0 {
            return Err(Error::InvalidInput("invalid replica snapshot identity"));
        }
        Ok(Self { source, policy_revision })
    }
    pub fn accepts(self, source: SnapshotToken, policy_revision: u64) -> bool {
        self.source == source && self.policy_revision == policy_revision
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert canonical receipt fixtures.")]
mod tests {
    use super::*;

    // Decode exact wire receipts: no unknown fields, zero generations, old
    // receipts, or rounded uint64 epochs can qualify a source snapshot.
    #[test]
    fn receipt_decoder_requires_complete_snapshot_identity() {
        let receipt = ReplicaReceipt {
            namespace: NamespaceReceipt::Unavailable,
            durable: DurableRevision { root: None, revision: u64::MAX, policy_revision: 1 },
            source: SnapshotToken { generation: Id::from_bytes([1; 16]), epoch: u64::MAX },
        };
        let encoded = serde_json::to_value(&receipt).unwrap();
        assert_eq!(serde_json::from_value::<ReplicaReceipt>(encoded.clone()).unwrap(), receipt);
        let mut candidates = Vec::new();
        let mut unknown = encoded.clone();
        unknown["grant"] = serde_json::json!(true);
        candidates.push(unknown);
        let mut zero = encoded.clone();
        zero["source"]["generation"] = serde_json::json!(Id::from_bytes([0; 16]));
        candidates.push(zero);
        let mut zero_policy = encoded.clone();
        zero_policy["policy_revision"] = serde_json::json!(0);
        candidates.push(zero_policy);
        let mut old = encoded.clone();
        old.as_object_mut().unwrap().remove("source");
        candidates.push(old);
        let mut unknown_source = encoded.clone();
        unknown_source["source"]["durable"] = serde_json::json!(true);
        candidates.push(unknown_source);
        for candidate in candidates {
            assert!(serde_json::from_value::<ReplicaReceipt>(candidate).is_err());
        }
        let text = serde_json::to_string(&receipt).unwrap();
        assert_eq!(serde_json::from_str::<ReplicaReceipt>(&text).unwrap(), receipt);
    }
}
