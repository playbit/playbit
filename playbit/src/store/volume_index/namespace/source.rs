//! Decode exact canonical sources into namespace facts. Callers still own
//! source authorization and causal admission; decoding alone qualifies nothing.
//! Register and history keys never contribute to immutable event identity.
use super::super::{EntryChange, EntryKind, EntryRevision};
use super::*;
use observations::Witness;
pub mod cut;

pub struct Declaration {
    entry: EntryId,
    clock: kvdb::Clock,
    digest: [u8; 32],
    created_revision: EntryId,
    kind: std::mem::Discriminant<EntryKind>,
    namespace_kind: Kind,
    namespace: Birth,
    origin: Origin,
    name: String,
}
pub struct Decoded {
    pub fact: Fact,
    pub witness: Witness,
}
impl Declaration {
    pub fn decode(entry: EntryId, clock: kvdb::Clock, bytes: &[u8]) -> Result<Self> {
        if clock.is_zero() {
            return Err(Error::InvalidInput("namespace declaration clock is zero"));
        }
        let info = super::super::info(bytes)?;
        let digest = super::super::creation_digest(entry, clock, bytes);
        let kind = std::mem::discriminant(&info.kind);
        let namespace_kind = match info.kind {
            EntryKind::Blob { .. } => Kind::File,
            EntryKind::Directory {} => Kind::Directory,
            EntryKind::KvDb {} | EntryKind::ObjectDb { .. } => Kind::Other,
        };
        // Schemas and ACLs were validated by the source decoder. Revision
        // dependency checks need only the immutable kind and creation binding.
        Ok(Self {
            entry,
            clock,
            digest,
            kind,
            namespace_kind,
            created_revision: info.created_revision,
            namespace: info.namespace,
            origin: info.origin,
            name: info.name,
        })
    }
    pub(crate) fn heap_bytes(&self) -> usize {
        self.name.capacity()
            + self.origin.label.as_ref().map_or(0, String::capacity)
            + size_of::<EntryId>()
                * (self.namespace.observed.capacity()
                    + self.namespace.deletions.capacity()
                    + self.namespace.departures.capacity()
                    + self
                        .namespace
                        .creation_context
                        .as_ref()
                        .map_or(0, |context| context.observed.capacity()))
    }
    pub fn witness(&self) -> Witness {
        Witness {
            entry: self.entry,
            revision: self.created_revision,
            source_digest: self.digest,
            clock: self.clock.as_u64(),
            birth: true,
            observed: self.namespace.observed.clone().into(),
        }
    }
    pub fn birth(&self) -> Result<Decoded> {
        let location = &self.namespace;
        let witness = self.witness();
        let fact = Fact {
            id: witness.id()?,
            clock: self.clock.as_u64(),
            observed: witness.observed.clone(),
            origin: self.origin.clone(),
            change: Change::Birth {
                creation_context: location.creation_context.clone().map(Into::into),
                entry: self.entry,
                parent: location.parent,
                name: self.name.clone(),
                kind: self.namespace_kind,
                automatic: location.automatic,
                deletions: location.deletions.clone(),
                departures: location.departures.clone(),
            },
        };
        fact.validate_shape()?;
        Ok(Decoded { fact, witness })
    }
    /// Only actual immutable revision bytes are accepted. The synthetic Created
    /// history wrapper is not a second birth and must use birth() instead.
    pub fn revision(&self, clock: kvdb::Clock, bytes: &[u8]) -> Result<Option<Decoded>> {
        self.revision_record(clock, bytes, super::super::revision(bytes)?)
    }
    // Only canonical decoded revisions from revision() or validate_source_record.
    fn revision_record(
        &self,
        clock: kvdb::Clock,
        bytes: &[u8],
        record: EntryRevision,
    ) -> Result<Option<Decoded>> {
        if record.origin_clock != clock.as_u64() {
            return Err(Error::AccessDenied("namespace revision origin clock changed"));
        }
        super::super::validate_dependency(
            self.kind,
            self.created_revision,
            self.digest,
            self.clock,
            &record,
        )?;
        let change = self.revision_change(&record)?;
        let Some(change) = change else { return Ok(None) };
        let witness = Witness {
            entry: self.entry,
            revision: record.revision,
            source_digest: *blake3::hash(bytes).as_bytes(),
            clock: clock.as_u64(),
            birth: false,
            observed: record.observed.into(),
        };
        let fact = Fact {
            id: witness.id()?,
            clock: clock.as_u64(),
            observed: witness.observed.clone(),
            origin: record.origin,
            change,
        };
        fact.validate_shape()?;
        Ok(Some(Decoded { fact, witness }))
    }
    /// Decode a physically bound namespace source; ACL/schema-only records do
    /// not become namespace observations during rejected-source reconciliation.
    pub fn source(&self, key: &[u8], clock: kvdb::Clock, bytes: &[u8]) -> Result<Option<Decoded>> {
        use super::super as index;
        if index::source_entry_id(key)? != self.entry {
            return Err(Error::AccessDenied("namespace source belongs to another declaration"));
        }
        if key == index::entry_key(self.entry) {
            if self.clock != clock
                || self.digest != index::creation_digest(self.entry, clock, bytes)
            {
                return Err(Error::AccessDenied("namespace declaration source changed"));
            }
            return self.birth().map(Some);
        }
        if key.first() == Some(&index::META_BYTE)
            && [index::NAME, index::CONTENT, index::PARENT, index::RETIRED, index::REVISION]
                .contains(&key[1])
        {
            index::validate_source(&kvdb::sync::Delta {
                key,
                local: kvdb::sync::EntState::Missing,
                remote: kvdb::sync::EntState::Present { clock, value: bytes },
            })?;
            return self.revision(clock, bytes);
        }
        Ok(None)
    }
    fn revision_change(&self, record: &EntryRevision) -> Result<Option<Change>> {
        let change = match &record.change {
            EntryChange::Name { name } => Change::Edit {
                entry: self.entry,
                parent: None,
                name: Some(name.clone()),
                content: false,
            },
            EntryChange::Delete {} => Change::Delete { entry: self.entry },
            EntryChange::Move { parent, name } => Change::Edit {
                entry: self.entry,
                parent: Some(*parent),
                name: name.clone(),
                content: false,
            },
            EntryChange::Content { .. } => {
                Change::Edit { entry: self.entry, parent: None, name: None, content: true }
            }
            // Schema publication changes no namespace assignment or file content.
            // Its ID must never enter a namespace observation context.
            EntryChange::Schema { .. } => return Ok(None),
            EntryChange::Created { .. } => {
                return Err(Error::InvalidInput("synthetic creation is not a namespace source"));
            }
        };
        Ok(Some(change))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests compare exact canonical source identities.")]
mod tests {
    use super::*;
    use crate::store::object_contract::{EntryAcl, Schema};
    use crate::store::volume_index::{DatabaseProtocol, EntryInfo};

    fn id(byte: u8) -> EntryId {
        EntryId::from_bytes([byte; 15]).unwrap()
    }
    fn declaration() -> (Declaration, Vec<u8>) {
        let info = EntryInfo {
            namespace: Birth::default(),
            origin: Origin { installation: Id::from_bytes([1; 16]), label: None },
            initial_permissions: EntryAcl::Inherit,
            created_revision: id(2),
            name: "records".into(),
            kind: EntryKind::ObjectDb { schema: Schema::default() },
            protocol: Some(DatabaseProtocol::Kvdb),
        };
        let bytes = info.canonical().unwrap();
        (Declaration::decode(id(1), kvdb::Clock::from_u64(10), &bytes).unwrap(), bytes)
    }
    #[test]
    fn source_identity_normalizes_copies_and_binds_payload_and_clock() {
        // Physical register/history keys are not identity. Different canonical
        // payloads or birth clocks must never qualify the same observation.
        let (declaration, bytes) = declaration();
        let birth = declaration.birth().unwrap();
        let later_birth =
            Declaration::decode(id(1), kvdb::Clock::from_u64(11), &bytes).unwrap().birth().unwrap();
        assert_ne!(birth.fact.id, later_birth.fact.id);
        let mut edit = EntryRevision {
            revision: id(3),
            creation: declaration.digest,
            origin_clock: 12,
            origin: declaration.origin.clone(),
            observed: vec![birth.fact.id],
            change: EntryChange::Name { name: "renamed".into() },
        };
        let bytes = edit.canonical().unwrap();
        let clock = kvdb::Clock::from_u64(12);
        let register = declaration.revision(clock, &bytes).unwrap().unwrap();
        let history = declaration.revision(clock, &bytes).unwrap().unwrap();
        assert_eq!(register.fact, history.fact);
        assert_eq!(register.witness, history.witness);
        assert_eq!(register.fact.id, register.witness.id().unwrap());
        let admission = admit(&[birth.fact, register.fact.clone(), history.fact]).unwrap();
        assert_eq!(admission.events.len(), 2);
        assert_eq!(admission.view.entries[&id(1)].name, "renamed");
        edit.change = EntryChange::Name { name: "other".into() };
        let other = declaration.revision(clock, &edit.canonical().unwrap()).unwrap().unwrap();
        assert_ne!(register.fact.id, other.fact.id);
        assert!(declaration.revision(kvdb::Clock::from_u64(13), &bytes).is_err());
        edit.creation = [0; 32];
        assert!(declaration.revision(clock, &edit.canonical().unwrap()).is_err());
    }
    #[test]
    fn schema_and_synthetic_history_never_introduce_namespace_dependencies() {
        // Schema history has no namespace effect; a synthetic Created wrapper
        // must not fabricate a second event for the same immutable declaration.
        let (declaration, bytes) = declaration();
        let birth = declaration.birth().unwrap();
        let mut revision = EntryRevision {
            revision: id(3),
            creation: declaration.digest,
            origin_clock: 12,
            origin: declaration.origin.clone(),
            observed: vec![birth.fact.id],
            change: EntryChange::Schema { declaration: Schema::default() },
        };
        assert!(
            declaration
                .revision(kvdb::Clock::from_u64(12), &revision.canonical().unwrap())
                .unwrap()
                .is_none()
        );
        revision.change =
            EntryChange::Created { info: Box::new(super::super::super::info(&bytes).unwrap()) };
        assert!(
            declaration
                .revision(kvdb::Clock::from_u64(12), &revision.canonical().unwrap())
                .is_err()
        );
        let mut malformed = super::super::super::info(&bytes).unwrap();
        malformed.namespace.observed = vec![id(4), id(3)];
        assert!(malformed.canonical().is_err());
        malformed.namespace = Birth { automatic: true, ..Birth::default() };
        assert!(malformed.canonical().is_err());
    }
}
