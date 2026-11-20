//! Bootstrap policy admission. Charge before decoding allocates any containers.
use super::*;
use kvdb::storage::ValueRead;
use std::io::Read;

const BASE_BYTES: usize = 16 * 1024;

// Conservative per JSON member/element charges include BTree nodes at minimum
// occupancy, Vec growth, serde's tagged-enum intermediates and validation sets.
// Strings reserve four times their encoded length for decode and retained data.
struct Budget {
    used: usize,
    limit: usize,
}
impl Budget {
    fn new(limit: usize) -> Result<Self> {
        if !(BASE_BYTES..=MetadataPolicy::MAX_MATERIALIZED_BYTES).contains(&limit) {
            return Err(Error::LimitExceeded);
        }
        Ok(Self { used: BASE_BYTES, limit })
    }
    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.used = self
            .used
            .checked_add(bytes)
            .filter(|n| *n <= self.limit)
            .ok_or(Error::LimitExceeded)?;
        assert!(self.used <= self.limit, "successful charge retains the policy budget");
        Ok(())
    }
    fn entry(&mut self, key: &[u8], value: &dyn kvdb::storage::ValueRead) -> Result<bool> {
        let bytes = key
            .len()
            .checked_mul(2)
            .and_then(|n| n.checked_add(512))
            .ok_or(Error::LimitExceeded)?;
        self.charge(bytes)?;
        if key == FORMAT_KEY || key.starts_with(&PROVISION_PREFIX) {
            self.json(StoredValue { reader: value, offset: 0 })?;
        } else {
            let raw = value.len().checked_mul(3).ok_or(Error::LimitExceeded)?;
            self.charge(raw)?;
            let mut bytes = vec![0; value.len()];
            value.read_exact_at(0, &mut bytes)?;
            let charge = record::charge(&bytes)?;
            self.charge(
                charge
                    .checked_sub(raw)
                    .ok_or(Error::Internal("record charge includes raw bytes"))?,
            )?;
        }
        Ok(true)
    }
    fn json(&mut self, mut input: impl Read) -> Result<()> {
        let mut string = false;
        let mut escape = false;
        let mut chunk = [0; 4096];
        loop {
            let count = input
                .read(&mut chunk)
                .map_err(|error| Error::read_failure("read stored catalog value", error))?;
            if count == 0 {
                return Ok(());
            }
            // This fixed chunk can charge at most 4096 * 512 bytes. No
            // decoder allocation occurs until the complete preflight succeeds.
            let mut charged = 0;
            for &byte in &chunk[..count] {
                if string {
                    charged += 4;
                    if escape {
                        escape = false;
                    } else if byte == b'\\' {
                        escape = true;
                    } else if byte == b'"' {
                        string = false;
                    }
                } else {
                    match byte {
                        b'"' => {
                            string = true;
                            charged += 128;
                        }
                        b'{' | b'[' | b',' | b':' => charged += 512,
                        b' ' | b'\t' | b'\r' | b'\n' => {}
                        _ => charged += 4,
                    }
                }
            }
            self.charge(charged)?;
        }
    }
}

pub(super) fn preflight(bytes: &[u8]) -> Result<()> {
    Budget::new(MetadataPolicy::MAX_MATERIALIZED_BYTES)?.charge(record::charge(bytes)?)
}

pub(super) fn index_record_charge(bytes: &[u8]) -> Result<usize> {
    let mut budget = Budget::new(MetadataPolicy::MAX_MATERIALIZED_BYTES)?;
    budget.charge(record::charge(bytes)?)?;
    Ok(budget.used - BASE_BYTES)
}
pub(super) fn json_projection_charge(bytes: &[u8]) -> Result<usize> {
    let mut budget = Budget::new(MetadataPolicy::MAX_MATERIALIZED_BYTES)?;
    budget.json(bytes)?;
    Ok(budget.used - BASE_BYTES)
}

struct StoredValue<'a> {
    reader: &'a dyn kvdb::storage::ValueRead,
    offset: usize,
}
impl Read for StoredValue<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let count = self.reader.read_at(self.offset, out).map_err(std::io::Error::other)?;
        assert!(count <= out.len(), "value reader respects destination capacity");
        self.offset = self.offset.checked_add(count).expect("value read offset fits usize");
        Ok(count)
    }
}
fn decode_value<T: record::Codec, S: Storage>(tx: &kvdb::ReadTxn<'_, S>, key: &[u8]) -> Result<T> {
    let reader = tx.value_reader(key)?.ok_or(Error::corrupt("catalog value is missing"))?;
    if reader.len() > MetadataPolicy::MAX_MATERIALIZED_BYTES {
        return Err(Error::LimitExceeded);
    }
    let mut bytes = vec![0; reader.len()];
    reader.read_exact_at(0, &mut bytes)?;
    record::decode(&bytes)
}
fn decode_reader<T: DeserializeOwned>(reader: &dyn kvdb::storage::ValueRead) -> Result<T> {
    // Retain one snapshot-backed page guard through this pass. BufReader avoids
    // one storage lookup per serde byte, without copying the complete value.
    let reader = std::io::BufReader::with_capacity(4096, StoredValue { reader, offset: 0 });
    serde_json::from_reader(reader).map_err(Error::decode::<T>)
}
impl MetadataPolicy {
    /// Admit incoming metadata allocations before a resolver decodes schemas or
    /// ACLs. This accounts for all present values, including losing entries.
    /// Framing, identity, schema, authority and pending-catalog validation remain
    /// separate; passing this allocation check does not authorize a merge.
    pub fn validate_delta_materialization(bytes: &[u8], materialized_bytes: usize) -> Result<()> {
        let mut budget = Budget::new(materialized_bytes)?;
        let mut rejected = None;
        kvdb::sync::visit_delta(bytes, crate::store::MAX_REPLICA_DELTA_BYTES, |key, state| {
            if rejected.is_some() {
                return;
            }
            if let kvdb::sync::EntState::Present { value, .. } = state
                && let Err(error) = budget.entry(key, &kvdb::storage::SliceValue(value))
            {
                rejected = Some(error);
            }
        })?;
        rejected.map_or(Ok(()), Err)
    }

    /// Require the effective pending metadata to fit the same materialization
    /// budget as read_bounded before any durable commit. This does not replace
    /// schema/ACL validation or grant authority to the writer.
    pub fn validate_pending(
        tx: &kvdb::WriteTxn<'_, impl Storage>,
        materialized_bytes: usize,
    ) -> Result<()> {
        let _timing = crate::trace::Span::new("storage.pending_policy_capacity");
        let mut budget = Budget::new(materialized_bytes)?;
        validate_pending_format(tx)?;
        tx.visit(&VOLUME_KEY, false, |key, value| {
            #[cfg(test)]
            observe_policy_visit();
            if key >= &POLICY_END[..] {
                return Ok(false);
            }
            if is_policy_key(key) { budget.entry(key, value) } else { Ok(true) }
        })
    }

    /// Check compatibility without materializing policy or entry records.
    pub fn validate_format(tx: &kvdb::ReadTxn<'_, impl Storage>) -> Result<()> {
        let format = tx.value_reader(&FORMAT_KEY)?.ok_or(Error::UnsupportedVolumeFormat)?;
        validate_format_reader(&format)
    }

    /// Preflight the complete snapshot before materializing the first member.
    /// This is a bootstrap allocation limit, not a kvdb key/value size limit.
    pub fn read_bounded(database: &Db<impl Storage>, materialized_bytes: usize) -> Result<Self> {
        Self::read_snapshot(&database.begin_read()?, materialized_bytes)
    }
    pub fn read_snapshot(
        tx: &kvdb::ReadTxn<'_, impl Storage>,
        materialized_bytes: usize,
    ) -> Result<Self> {
        let mut budget = Budget::new(materialized_bytes)?;
        Self::validate_format(tx)?;
        tx.visit(&VOLUME_KEY, false, |key, value| {
            #[cfg(test)]
            observe_policy_visit();
            if key >= &POLICY_END[..] {
                return Ok(false);
            }
            if is_policy_key(key) { budget.entry(key, value) } else { Ok(true) }
        })?;
        let mut policy = Self {
            revision: tx.clock()?.as_u64(),
            volume: decode_value(tx, &VOLUME_KEY)?,
            permissions: decode_value(tx, &DEFAULT_PERMISSIONS_KEY)?,
            provisioned_versions: BTreeSet::new(),
        };
        tx.visit(&PROVISION_PREFIX, false, |key, value| -> Result<bool> {
            if !key.starts_with(&PROVISION_PREFIX) {
                return Ok(false);
            }
            let version = std::str::from_utf8(&key[2..])
                .map_err(|_| Error::corrupt("provision version is not UTF-8"))?;
            validate_version(version)?;
            if !decode_reader::<bool>(value)? {
                return Err(Error::corrupt("provision marker must be true"));
            }
            policy.provisioned_versions.insert(version.to_owned());
            Ok(true)
        })?;
        policy.permissions.validate()?;
        validate_volume_label(&policy.volume.name)?;
        Ok(policy)
    }
}

#[cfg(test)]
std::thread_local! {
    static POLICY_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
fn observe_policy_visit() {
    POLICY_VISITS.with(|visits| visits.set(visits.get() + 1));
}
#[cfg(test)]
mod tests;
