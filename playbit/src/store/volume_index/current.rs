//! Fixed point reads of qualified original-entry fields. Source inspection is
//! explicit and is reserved for staging/recovery, before publication qualifies it.
use super::*;
use namespace::source::cut::{Sources, current::Record};

type Value = (kvdb::Clock, Vec<u8>);

pub(super) struct Fields {
    pub birth: Value,
    name: Option<Value>,
    content: Option<Value>,
    parent: Option<Value>,
    deletion: Option<Value>,
    selected: Option<Record>,
}
impl Fields {
    pub fn current(tx: &impl Sources, id: EntryId) -> Result<Self> {
        let selected = Record::read(tx, id)?.ok_or(Error::NotFound("qualified entry"))?;
        Ok(Self {
            birth: selected.birth.read(tx)?,
            name: selected.name.map(|source| source.read(tx)).transpose()?,
            content: selected.content.map(|source| source.read(tx)).transpose()?,
            parent: selected.parent.map(|source| source.read(tx)).transpose()?,
            deletion: selected.deletion.map(|source| source.read(tx)).transpose()?,
            selected: Some(selected),
        })
    }

    pub fn source(tx: &impl Sources, id: EntryId) -> Result<Self> {
        Ok(Self {
            birth: tx
                .source(&entry_key(id), MAX_ENTRY_RECORD_BYTES)?
                .ok_or(Error::NotFound("entry source"))?,
            name: tx.source(&meta_key(NAME, id), MAX_ENTRY_RECORD_BYTES)?,
            content: tx.source(&meta_key(CONTENT, id), MAX_ENTRY_RECORD_BYTES)?,
            parent: tx.source(&meta_key(PARENT, id), MAX_ENTRY_RECORD_BYTES)?,
            deletion: tx.source(&meta_key(RETIRED, id), MAX_ENTRY_RECORD_BYTES)?,
            selected: None,
        })
    }

    pub fn assemble(&self, id: EntryId) -> Result<VolumeEntry> {
        let birth = (self.birth.0, self.birth.1.as_slice());
        let values = [&self.name, &self.content, &self.parent, &self.deletion]
            .map(|field| field.as_ref().map(|(clock, bytes)| (*clock, bytes.as_slice())));
        let deleted = validate_deletion(id, birth, [values[0], values[1], values[2]], values[3])?;
        let (info, name_revision, content_revision) =
            assemble(id, birth, values[0], values[1], values[2])?;
        if let Some(selected) = self.selected {
            if info.created_revision != selected.birth.revision
                || name_revision != selected.name.unwrap_or(selected.birth).revision
                || content_revision != selected.content.unwrap_or(selected.birth).revision
            {
                return Err(Error::corrupt("current entry reference differs from source revision"));
            }
            for (source, value) in [(selected.parent, values[2]), (selected.deletion, values[3])] {
                if let (Some(source), Some((_, bytes))) = (source, value)
                    && revision(bytes)?.revision != source.revision
                {
                    return Err(Error::corrupt(
                        "current entry reference differs from source revision",
                    ));
                }
            }
        }
        Ok(VolumeEntry { id, info, name_revision, content_revision, deleted, name_conflict: false })
    }
}

/// Identity checks read only the qualified birth and direct deletion. Name,
/// content, additive schema, and ancestor visibility cannot grant or deny them.
pub(super) fn identity(tx: &impl Sources, id: EntryId) -> Result<(EntryInfo, bool)> {
    Identity::read(tx, id)?.ok_or(Error::NotFound("qualified entry"))?.decode(id, 0)
}

/// Keep bounded source bytes separate from decoding so header pages can stop
/// before the next identity exceeds their cumulative decode allowance.
pub(super) struct Identity {
    birth: Value,
    deletion: Option<Value>,
    selected: Record,
}
impl Identity {
    pub fn read(tx: &impl Sources, id: EntryId) -> Result<Option<Self>> {
        let Some(selected) = Record::read(tx, id)? else { return Ok(None) };
        Ok(Some(Self {
            birth: selected.birth.read(tx)?,
            deletion: selected.deletion.map(|source| source.read(tx)).transpose()?,
            selected,
        }))
    }
    pub fn source_bytes(&self) -> usize {
        self.birth.1.len() + self.deletion.as_ref().map_or(0, |(_, bytes)| bytes.len())
    }
    pub fn decode(&self, id: EntryId, retained: usize) -> Result<(EntryInfo, bool)> {
        let birth = (self.birth.0, self.birth.1.as_slice());
        let deletion = self.deletion.as_ref().map(|(clock, bytes)| (*clock, bytes.as_slice()));
        let (_, charged) = source_charge(&[Some(birth), deletion])?;
        if charged
            .checked_add(retained)
            .is_none_or(|total| total > MetadataPolicy::MAX_MATERIALIZED_BYTES)
        {
            return Err(Error::LimitExceeded);
        }
        let deleted = validate_deletion(id, birth, [None; 3], deletion)?;
        let initial = info(&self.birth.1)?;
        if initial.created_revision != self.selected.birth.revision {
            return Err(Error::corrupt("current identity reference differs from source revision"));
        }
        if let Some((_, bytes)) = &self.deletion
            && Some(revision(bytes)?.revision)
                != self.selected.deletion.map(|source| source.revision)
        {
            return Err(Error::corrupt("current deletion reference differs from source revision"));
        }
        Ok((initial, deleted))
    }
}

/// Read the exact qualified body without names, ancestor traversal, or additive
/// ObjectDb schema. Retained readiness is meaningful even after direct deletion.
pub(super) fn body(tx: &impl Sources, id: EntryId) -> Result<EntryKind> {
    let selected = Record::read(tx, id)?.ok_or(Error::NotFound("qualified entry"))?;
    let birth = selected.birth.read(tx)?;
    let content = selected.content.map(|source| source.read(tx)).transpose()?;
    let (info, _, revision) = assemble(
        id,
        (birth.0, &birth.1),
        None,
        content.as_ref().map(|(clock, bytes)| (*clock, bytes.as_slice())),
        None,
    )?;
    if info.created_revision != selected.birth.revision
        || revision != selected.content.unwrap_or(selected.birth).revision
    {
        return Err(Error::corrupt("current body reference differs from source revision"));
    }
    Ok(info.kind)
}

/// Detached receipt and rejected recovery inspect the received source body.
/// This does not qualify the entry or authorize access to its backing.
pub(super) fn source_body(tx: &impl Sources, id: EntryId) -> Result<EntryKind> {
    let birth = tx
        .source(&entry_key(id), MAX_ENTRY_RECORD_BYTES)?
        .ok_or(Error::NotFound("entry source"))?;
    let content = tx.source(&meta_key(CONTENT, id), MAX_ENTRY_RECORD_BYTES)?;
    Ok(assemble(
        id,
        (birth.0, &birth.1),
        None,
        content.as_ref().map(|(clock, bytes)| (*clock, bytes.as_slice())),
        None,
    )?
    .0
    .kind)
}

#[cfg(feature = "connected-engine")]
pub(crate) struct Retirement {
    entry: EntryId,
    revision: EntryId,
    clock: kvdb::Clock,
    bytes: Vec<u8>,
}

#[cfg(feature = "connected-engine")]
impl Retirement {
    pub(crate) fn copy_into(&self, tx: &mut WriteTxn<'_, impl Storage>) -> Result<()> {
        tx.put_with_clock(&meta_key(RETIRED, self.entry), &self.bytes, self.clock)?;
        tx.put_with_clock(&revision_key(self.entry, self.revision), &self.bytes, self.clock)?;
        Ok(())
    }
}

impl<S: Storage> IndexSnapshot<'_, S> {
    /// A selected replay can reach AP before a sibling edit in its observed
    /// context. Match the raw head to the exact locally authored deletion
    /// without qualifying its namespace event.
    #[cfg(any(feature = "local-store", test))]
    pub(crate) fn matches_authored_retirement(
        &self,
        entry: EntryId,
        record: Record,
        captured: &Self,
        confirmed: &[EntryId],
    ) -> Result<bool> {
        let Some(deletion) = record.deletion else { return Ok(false) };
        let key = meta_key(RETIRED, entry);
        let Some((clock, bytes)) = captured.tx.get_with_clock(&key)? else { return Ok(false) };
        let local = deletion.read(&self.tx)?;
        if clock != local.0 || bytes.as_ref() != local.1 {
            return Ok(false);
        }
        drop(bytes);
        let birth = record.birth.read(&self.tx)?;
        source_charge(&[Some((birth.0, &birth.1)), Some((clock, &local.1))])?;
        let witness = deletion_witness(entry, (birth.0, &birth.1), (clock, &local.1))?;
        drop(birth);
        if witness.id()? != deletion.event_id()
            || namespace::observations::source_status(&self.tx, &witness, local.1.len())?
                != Some(namespace::observations::Status::Authored)
        {
            return Ok(false);
        }
        drop(witness);
        drop(local);
        // A raw head may supersede a different AP-confirmed deletion. That
        // deletion's immutable history copy is part of the authenticated cut.
        let prefix = meta_key(REVISION, entry);
        let mut cursor = prefix.to_vec();
        let mut scan = namespace::source::cut::Scan::default();
        while let Some(key) = captured.tx.next_source_key(&cursor, true)? {
            if !key.starts_with(&prefix) {
                break;
            }
            if let Some((_, bytes)) = scan.read(&captured.tx, &key)? {
                let candidate = revision(&bytes)?;
                if matches!(candidate.change, EntryChange::Delete {}) {
                    let digest = *blake3::hash(&bytes).as_bytes();
                    let event = namespace::event_id(entry, candidate.revision, digest)?;
                    if confirmed.binary_search(&event).is_ok() {
                        return Ok(false);
                    }
                }
            }
            cursor = key;
        }
        Ok(true)
    }

    /// Selected replicas may lack the other entries observed by a local edit.
    /// Only an exact Authored deletion can supplement a qualified local head.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn selected_retirement(
        &self,
        id: EntryId,
    ) -> Result<Option<([EntryId; 2], Retirement)>> {
        use namespace::observations::{Status, source_status};
        let Some(current) = Record::read(&self.tx, id)? else { return Ok(None) };
        let (clock, bytes) = if let Some(deletion) = current.deletion {
            deletion.read(&self.tx)?
        } else {
            let Some((clock, bytes)) = self.tx.get_with_clock(&meta_key(RETIRED, id))? else {
                return Ok(None);
            };
            (clock, bytes.to_vec())
        };
        let birth = current.birth.read(&self.tx)?;
        source_charge(&[Some((birth.0, &birth.1)), Some((clock, &bytes))])?;
        let source = deletion_witness(id, (birth.0, &birth.1), (clock, &bytes))?;
        if current.deletion.is_none()
            && (source_status(&self.tx, &source, bytes.len())? != Some(Status::Authored)
                || !self.tx.get_with_clock(&revision_key(id, source.revision))?.is_some_and(
                    |(other_clock, other)| other_clock == clock && other.as_ref() == bytes,
                ))
        {
            return Ok(None);
        }
        let events = [current.birth.event_id(), source.id()?];
        Ok(Some((events, Retirement { entry: id, revision: source.revision, clock, bytes })))
    }

    #[cfg(feature = "connected-engine")]
    pub(crate) fn source_birth_event(&self, id: EntryId) -> Result<EntryId> {
        let (clock, bytes) =
            self.tx.get_with_clock(&entry_key(id))?.ok_or(Error::NotFound("entry birth"))?;
        namespace::source::Declaration::decode(id, clock, &bytes)?.witness().id()
    }

    #[cfg(feature = "connected-engine")]
    pub(crate) fn source_retirement_event(&self, id: EntryId) -> Result<EntryId> {
        let key = meta_key(RETIRED, id);
        let (clock, bytes) =
            self.tx.get_with_clock(&key)?.ok_or(Error::NotFound("entry deletion"))?;
        let (birth_clock, birth) =
            self.tx.get_with_clock(&entry_key(id))?.ok_or(Error::NotFound("entry birth"))?;
        source_charge(&[Some((birth_clock, &birth)), Some((clock, &bytes))])?;
        deletion_witness(id, (birth_clock, &birth), (clock, &bytes))?.id()
    }

    /// A local deletion can precede its backing's first upload. Only the
    /// qualified deletion for the captured birth can retire that AP backing.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn retirement_for(
        &self,
        id: EntryId,
        volume: &Volume,
        actor: DataActor,
        local_defaults: &Acl,
        captured: &IndexSnapshot<'_, S>,
        captured_defaults: &Acl,
    ) -> Result<Option<Retirement>> {
        let Some(current) = Record::read(&self.tx, id)? else { return Ok(None) };
        let Some(deletion) = current.deletion else { return Ok(None) };
        // Retirement is a Write. A fresh AP contraction takes precedence over
        // retained control bytes, even on the owner's device.
        if !self.source_granted(volume, local_defaults, actor, id, Permission::Write)?
            || !captured.source_granted(volume, captured_defaults, actor, id, Permission::Write)?
        {
            return Ok(None);
        }
        let birth = current.birth.read(&self.tx)?;
        if !captured
            .tx
            .get_with_clock(&entry_key(id))?
            .is_some_and(|(clock, value)| clock == birth.0 && value.as_ref() == birth.1)
        {
            return Ok(None);
        }
        let (clock, bytes) = deletion.read(&self.tx)?;
        validate_deletion(id, (birth.0, &birth.1), [None; 3], Some((clock, &bytes)))?;
        Ok(Some(Retirement { entry: id, revision: deletion.revision, clock, bytes }))
    }

    /// The exact qualified sources of the displayed reference and location.
    /// Directory coalescence may select another original entry's name/parent.
    #[cfg(any(feature = "local-store", test))]
    pub(crate) fn provider_sources(&self, id: EntryId) -> Result<[Option<EntryId>; 7]> {
        let current = Record::read(&self.tx, id)?.ok_or(Error::NotFound("qualified entry"))?;
        let visible = self.namespace_entry(id)?;
        Ok([
            Some(current.birth.event_id()),
            current.name.map(|source| source.event_id()),
            current.parent.map(|source| source.event_id()),
            current.content.map(|source| source.event_id()),
            current.deletion.map(|source| source.event_id()),
            Some(visible.name_event),
            Some(visible.parent_event),
        ])
    }
    pub fn entry(&self, id: EntryId) -> Result<VolumeEntry> {
        self.entry_fields(id, Fields::current(&self.tx, id)?)
    }

    /// Inspect unqualified source registers for detached staging or explicit
    /// rejected-state recovery. This never grants app access or a current body.
    pub fn source_entry(&self, id: EntryId) -> Result<VolumeEntry> {
        self.entry_fields(id, Fields::source(&self.tx, id)?)
    }

    fn entry_fields(&self, id: EntryId, fields: Fields) -> Result<VolumeEntry> {
        let mut entry = fields.assemble(id)?;
        if matches!(entry.info.kind, EntryKind::ObjectDb { .. }) {
            let (schema, _, _) = schema::read(&self.tx, id, (fields.birth.0, &fields.birth.1))?;
            entry.info.kind = EntryKind::ObjectDb { schema };
        }
        entry.name_conflict = if let Some(selected) = fields.selected {
            selected.name_conflict
        } else {
            let winner =
                self.tx.local_get(&name_key(entry.info.namespace.parent, &entry.info.name))?;
            !entry.deleted && winner.as_ref().map(|bytes| id_bytes(bytes)).transpose()? != Some(id)
        };
        Ok(entry)
    }
}
