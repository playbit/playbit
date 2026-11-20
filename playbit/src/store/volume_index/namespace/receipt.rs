//! Compact authenticated receipt plus a separately transferred acceptance set.
//! The receipt is meaningful only with its enclosing source cut, projected root,
//! policy revision and authenticated AP transport. IDs never reveal payloads.
use super::*;

pub const MAX_EVIDENCE_BYTES: usize = 8 + MAX_FACTS * 16;
const MAGIC: [u8; 4] = *b"NSA\x01";
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Receipt {
    Unavailable,
    Accepted { entry: Option<EntryId>, count: u32, digest: [u8; 32] },
}
impl Receipt {
    pub fn validate(&self) -> Result<()> {
        if let Self::Accepted { count, .. } = self
            && *count as usize > MAX_FACTS
        {
            return Err(Error::LimitExceeded);
        }
        Ok(())
    }
    pub fn new(
        entry: Option<EntryId>,
        accepted: &BTreeMap<EntryId, Assignments>,
    ) -> Result<(Self, Vec<u8>)> {
        if accepted.len() > MAX_FACTS {
            return Err(Error::LimitExceeded);
        }
        let count = accepted.len() as u32;
        let mut bytes = Vec::with_capacity(8 + accepted.len() * 16);
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&count.to_le_bytes());
        for (id, assignment) in accepted {
            bytes.extend_from_slice(id.as_bytes());
            bytes.push(assignment.bits());
        }
        let digest = *blake3::hash(&bytes).as_bytes();
        Ok((Self::Accepted { entry, count, digest }, bytes))
    }
    pub fn verify(
        &self,
        entry: Option<EntryId>,
        bytes: &[u8],
    ) -> Result<BTreeMap<EntryId, Assignments>> {
        self.validate()?;
        let Self::Accepted { entry: scope, count, digest } = self else {
            return Err(Error::AccessDenied("namespace acceptance receipt is unavailable"));
        };
        if *scope != entry
            || bytes.len() != 8 + *count as usize * 16
            || bytes.get(..4) != Some(&MAGIC)
            || bytes[4..8] != count.to_le_bytes()
            || blake3::hash(bytes).as_bytes() != digest
        {
            return Err(Error::InvalidInput("namespace acceptance evidence differs from receipt"));
        }
        let mut accepted = BTreeMap::new();
        let mut prior = None;
        for bytes in bytes[8..].as_chunks::<16>().0 {
            let id =
                EntryId::from_bytes(bytes[..15].try_into().expect("fixed-width namespace ID"))?;
            if prior.is_some_and(|prior| prior >= id) {
                return Err(Error::InvalidInput("namespace acceptance IDs are not ordered"));
            }
            accepted.insert(id, Assignments::from_bits(bytes[15])?);
            prior = Some(id);
        }
        Ok(accepted)
    }
}

const EVIDENCE_KEY: &[u8] = b"received-namespace-evidence";
const RECEIPT_KEY: &[u8] = b"received-namespace-receipt";

/// Retain authenticated evidence in a detached received index. These local rows
/// are outside the replicated root and must be captured before local-source merge.
pub fn retain(
    db: &kvdb::Db<impl kvdb::Storage>,
    receipt: &crate::store::ReplicaReceipt,
    entry: Option<EntryId>,
    bytes: &[u8],
) -> Result<()> {
    let accepted = receipt.namespace.verify(entry, bytes)?;
    let snapshot = db.begin_read()?;
    if snapshot.root_hash()? != receipt.durable.root
        || source::cut::delivered_ids(&snapshot, &accepted, entry)? != accepted
    {
        return Err(Error::AccessDenied("namespace receipt does not bind delivered sources"));
    }
    drop(snapshot);
    let encoded = crate::store::catalog::encode(receipt)?;
    let mut tx = db.begin_write(0)?;
    tx.local_indexes().put(EVIDENCE_KEY, bytes)?;
    tx.local_indexes().put(RECEIPT_KEY, &encoded)?;
    tx.commit()?;
    Ok(())
}

/// Exact payload witnesses from one authenticated projection, before it is mixed
/// with local facts. A publication must seal this result with its replay intent.
pub fn capture(
    tx: &kvdb::ReadTxn<'_, impl kvdb::Storage>,
    entry: Option<EntryId>,
) -> Result<Vec<observations::Observation>> {
    let bytes = tx.local_get(RECEIPT_KEY)?.ok_or(Error::NotFound("namespace receipt"))?;
    if bytes.len() > 4096 {
        return Err(Error::LimitExceeded);
    }
    let receipt: crate::store::ReplicaReceipt = crate::store::catalog::decode(&bytes)?;
    let bytes = tx.local_get(EVIDENCE_KEY)?.ok_or(Error::NotFound("namespace evidence"))?;
    if bytes.len() > MAX_EVIDENCE_BYTES || tx.root_hash()? != receipt.durable.root {
        return Err(Error::AccessDenied("namespace receipt projection changed"));
    }
    let accepted = receipt.namespace.verify(entry, &bytes)?;
    let witnesses = source::cut::delivered(tx, &accepted)?;
    if witnesses.len() != accepted.len()
        || witnesses.iter().any(|witness| entry.is_some_and(|entry| entry != witness.entry))
    {
        return Err(Error::AccessDenied("namespace receipt witness is absent or outside scope"));
    }
    witnesses
        .into_iter()
        .map(|source| {
            Ok(observations::Observation {
                assignment: Some(accepted[&source.id()?]),
                source,
                status: observations::Status::Confirmed,
            })
        })
        .collect::<Result<Vec<_>>>()
}

/// Test transports still bind full-cut acceptance to the exact projected root.
/// This supplies evidence only; callers explicitly qualify their AP fixture.
#[cfg(test)]
pub(crate) fn retain_fixture(
    db: &kvdb::Db<impl kvdb::Storage>,
    entry: Option<EntryId>,
    accepted: &BTreeMap<EntryId, Assignments>,
) -> Result<()> {
    let snapshot = db.begin_read()?;
    let delivered = source::cut::delivered_ids(&snapshot, accepted, entry)?;
    let (namespace, evidence) = Receipt::new(entry, &delivered)?;
    let receipt = crate::store::ReplicaReceipt {
        namespace,
        durable: crate::store::DurableRevision {
            root: snapshot.root_hash()?,
            revision: 1,
            policy_revision: 1,
        },
        source: crate::store::SnapshotToken { generation: Id::from_bytes([42; 16]), epoch: 1 },
    };
    drop(snapshot);
    retain(db, &receipt, entry, &evidence)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests alter exact authenticated evidence boundaries.")]
mod tests {
    use super::*;
    #[test]
    fn acceptance_evidence_binds_scope_count_order_and_bytes() {
        // A compact receipt binds the exact separately delivered set; malformed
        // or differently scoped evidence cannot qualify an omitted source.
        let first = EntryId::from_bytes([1; 15]).unwrap();
        let second = EntryId::from_bytes([2; 15]).unwrap();
        let ids = BTreeMap::from([
            (first, Assignments::default()),
            (second, Assignments { name: true, parent: false }),
        ]);
        let (receipt, bytes) = Receipt::new(Some(first), &ids).unwrap();
        assert_eq!(receipt.verify(Some(first), &bytes).unwrap(), ids);
        assert!(receipt.verify(None, &bytes).is_err());
        for end in [0, 4, 7, bytes.len() - 1] {
            assert!(receipt.verify(Some(first), &bytes[..end]).is_err());
        }
        let mut changed = bytes.clone();
        changed[8] ^= 1;
        assert!(receipt.verify(Some(first), &changed).is_err());
        let mut unordered = bytes.clone();
        unordered[8..23].copy_from_slice(second.as_bytes());
        unordered[24..39].copy_from_slice(first.as_bytes());
        let invalid = Receipt::Accepted {
            entry: Some(first),
            count: 2,
            digest: *blake3::hash(&unordered).as_bytes(),
        };
        assert!(invalid.verify(Some(first), &unordered).is_err());
        let mut invalid_mask = bytes.clone();
        invalid_mask[23] = 4;
        let invalid = Receipt::Accepted {
            entry: Some(first),
            count: 2,
            digest: *blake3::hash(&invalid_mask).as_bytes(),
        };
        assert!(invalid.verify(Some(first), &invalid_mask).is_err());
        let (empty, bytes) = Receipt::new(None, &BTreeMap::new()).unwrap();
        assert!(empty.verify(None, &bytes).unwrap().is_empty());
        assert!(Receipt::Unavailable.verify(None, &bytes).is_err());
    }
}
