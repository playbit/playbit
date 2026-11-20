//! Additive schema identity facts. Each declaration has its own source key;
//! deprecation is a separate permanent fact. Names never identify declarations.
use super::*;
use crate::store::object_contract::{FieldDeclaration, TypeDeclaration, ValueType};
use std::collections::BTreeMap;

pub(super) const TYPE: u8 = 9;
pub(super) const FIELD: u8 = 10;
pub(super) const DEPRECATED: u8 = 11;
const MAX_FACTS: usize = 1024;
// Three typed schema copies and BTree identity-validation nodes. A new type
// also reserves its three small field-map roots. Names cover growth plus copies.
const FIELD_RESIDENT_BYTES: usize = 768;
const TYPE_RESIDENT_BYTES: usize = 4096;

#[cfg(test)]
thread_local! {
    pub(super) static MATERIALIZATION_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn admitted_charge(current: usize, added: usize) -> Result<usize> {
    current
        .checked_add(added)
        .filter(|next| *next <= MetadataPolicy::MAX_MATERIALIZED_BYTES)
        .ok_or(Error::LimitExceeded)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "item", rename_all = "snake_case", deny_unknown_fields)]
enum Declaration {
    Type { name: String },
    Field { name: String, value_type: ValueType, repeated: bool },
    Deprecated,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Fact {
    #[serde(with = "anchor")]
    creation: [u8; 32],
    declaration: Declaration,
}
record::fields!(Fact { 1 => creation, 2 => declaration });
impl record::Codec for Declaration {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Type { name } => {
                record::variant(out, 1);
                record::put(out, 1, name)
            }
            Self::Field { name, value_type, repeated } => {
                record::variant(out, 2);
                record::put(out, 1, name)?;
                record::put(out, 2, value_type)?;
                record::put(out, 3, repeated)
            }
            Self::Deprecated => {
                record::variant(out, 3);
                Ok(())
            }
        }
    }
    fn read(value: record::View<'_>, mode: record::Mode) -> Result<Self> {
        let (tag, body) = value.variant()?;
        match tag {
            1 => Ok(Self::Type { name: body.fields(mode, &[1])?.required(1, mode)? }),
            2 => {
                let fields = body.fields(mode, &[1, 2, 3])?;
                Ok(Self::Field {
                    name: fields.required(1, mode)?,
                    value_type: fields.required(2, mode)?,
                    repeated: fields.required(3, mode)?,
                })
            }
            3 => {
                body.fields(mode, &[])?;
                Ok(Self::Deprecated)
            }
            _ => Err(Error::InvalidInput("unsupported schema declaration")),
        }
    }
}

mod anchor {
    use serde::{
        Deserializer, Serializer,
        de::{self, Visitor},
    };
    use std::fmt;

    pub fn serialize<S: Serializer>(value: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(blake3::Hash::from_bytes(*value).to_hex().as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 32], D::Error> {
        struct Anchor;
        impl Visitor<'_> for Anchor {
            type Value = [u8; 32];
            fn expecting(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
                out.write_str("a 64-byte lowercase hexadecimal creation digest")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                fn digit(byte: u8) -> Option<u8> {
                    match byte {
                        b'0'..=b'9' => Some(byte - b'0'),
                        b'a'..=b'f' => Some(byte - b'a' + 10),
                        _ => None,
                    }
                }
                if value.len() != 64 {
                    return Err(E::custom("invalid creation digest width"));
                }
                let mut bytes = [0; 32];
                for (out, pair) in bytes.iter_mut().zip(value.as_bytes().as_chunks::<2>().0) {
                    *out = digit(pair[0])
                        .zip(digit(pair[1]))
                        .map(|(high, low)| (high << 4) | low)
                        .ok_or_else(|| E::custom("invalid creation digest encoding"))?;
                }
                Ok(bytes)
            }
        }
        deserializer.deserialize_str(Anchor)
    }
}

pub(super) fn is_key(key: &[u8]) -> bool {
    key.len() >= 2 && key[0] == META_BYTE && [TYPE, FIELD, DEPRECATED].contains(&key[1])
}

pub(super) fn key_entry(key: &[u8]) -> Result<EntryId> {
    match (key.get(1), key.len()) {
        (Some(&TYPE), 33) | (Some(&FIELD), 49) | (Some(&DEPRECATED), 33 | 49) => {
            id_bytes(&key[2..17])
        }
        _ => Err(Error::AccessDenied("invalid schema fact key")),
    }
}

fn type_key(kind: u8, entry: EntryId, type_id: Id) -> [u8; 33] {
    let mut key = [0; 33];
    key[..17].copy_from_slice(&meta_key(kind, entry));
    key[17..].copy_from_slice(type_id.as_bytes());
    key
}

fn field_key(kind: u8, entry: EntryId, type_id: Id, field_id: Id) -> [u8; 49] {
    let mut key = [0; 49];
    key[..33].copy_from_slice(&type_key(kind, entry, type_id));
    key[33..].copy_from_slice(field_id.as_bytes());
    key
}

fn fact(key: &[u8], bytes: &[u8]) -> Result<Fact> {
    key_entry(key)?;
    if bytes.len() > MAX_ENTRY_RECORD_BYTES {
        return Err(Error::LimitExceeded);
    }
    crate::store::catalog::preflight_index_record(bytes)?;
    let fact: Fact = decode(bytes)?;
    match (&fact.declaration, key[1]) {
        (Declaration::Type { name }, TYPE) | (Declaration::Field { name, .. }, FIELD)
            if !name.trim().is_empty() && name.len() <= 255 && !name.contains('\0') => {}
        (Declaration::Deprecated, DEPRECATED) => {}
        _ => return Err(Error::InvalidInput("schema fact disagrees with its key")),
    }
    Ok(fact)
}

pub(super) fn validate_value(key: &[u8], bytes: &[u8]) -> Result<()> {
    fact(key, bytes).map(|_| ())
}

pub(super) fn validate_delta(delta: &Delta<'_>) -> Result<()> {
    let EntState::Present { value, clock } = delta.remote else {
        return if matches!(delta.remote, EntState::Missing) {
            Ok(())
        } else {
            Err(Error::AccessDenied("schema facts cannot be erased"))
        };
    };
    if clock.is_zero() {
        return Err(Error::AccessDenied("schema fact origin clock is zero"));
    }
    fact(delta.key, value)?;
    // Independent replicas may establish identical IDs/meanings concurrently.
    // Clock differences do not permit changing a declaration's meaning.
    if let EntState::Present { value: local, .. } = delta.local
        && local != value
    {
        return Err(Error::AccessDenied("schema identity or creation binding changed"));
    }
    Ok(())
}

struct Materializer {
    schema: Schema,
    creation: [u8; 32],
    clock: kvdb::Clock,
    charge: usize,
    facts: usize,
    pending: usize,
}

impl Materializer {
    fn new(entry: EntryId, declaration: SourceValue<'_>) -> Result<Self> {
        let (clock, bytes) = declaration;
        let EntryKind::ObjectDb { schema } = info(bytes)?.kind else {
            return Err(Error::AccessDenied("schema facts require an object database"));
        };
        Ok(Self {
            schema,
            clock,
            creation: creation_digest(entry, clock, bytes),
            charge: 16 * 1024 + crate::store::catalog::index_record_charge(bytes)?,
            facts: 0,
            pending: 0,
        })
    }

    fn observe(&mut self, key: &[u8], clock: kvdb::Clock, bytes: &[u8]) -> Result<()> {
        #[cfg(test)]
        MATERIALIZATION_VISITS.with(|visits| visits.set(visits.get() + 1));
        self.facts += 1;
        if self.facts > MAX_FACTS {
            return Err(Error::LimitExceeded);
        }
        // This complete decoder charge coexists with prior retained schema.
        // It is released before parsing the next fact, never retained per row.
        admitted_charge(self.charge, crate::store::catalog::index_record_charge(bytes)?)?;
        let fact = fact(key, bytes)?;
        let (resident, name_bytes) = match &fact.declaration {
            Declaration::Type { name } => (TYPE_RESIDENT_BYTES, name.len()),
            Declaration::Field { name, .. } => (FIELD_RESIDENT_BYTES, name.len()),
            Declaration::Deprecated => (FIELD_RESIDENT_BYTES, 0),
        };
        self.charge = admitted_charge(self.charge, resident + 4 * name_bytes)?;
        if fact.creation != self.creation || clock <= self.clock {
            return Err(Error::AccessDenied("schema fact does not follow its entry creation"));
        }
        let type_id = Id::from_bytes(key[17..33].try_into().expect("validated schema type width"));
        match fact.declaration {
            Declaration::Type { name } => self.add_type(type_id, name)?,
            Declaration::Field { name, value_type, repeated } => {
                let field = Id::from_bytes(key[33..49].try_into().expect("validated field width"));
                self.add_field(
                    type_id,
                    field,
                    FieldDeclaration { name, value_type, repeated, deprecated: false },
                )?;
            }
            Declaration::Deprecated => self.deprecate(type_id, key),
        }
        Ok(())
    }

    fn add_type(&mut self, id: Id, name: String) -> Result<()> {
        if let Some(current) = self.schema.types.get(&id) {
            if current.name != name {
                return Err(Error::AccessDenied("established type meaning changed"));
            }
        } else {
            self.schema
                .types
                .insert(id, TypeDeclaration { name, deprecated: false, fields: BTreeMap::new() });
        }
        Ok(())
    }

    fn add_field(&mut self, type_id: Id, field: Id, declaration: FieldDeclaration) -> Result<()> {
        let Some(current) = self.schema.types.get_mut(&type_id) else {
            self.pending += 1;
            return Ok(());
        };
        if let Some(previous) = current.fields.get(&field) {
            if previous.name != declaration.name
                || previous.value_type != declaration.value_type
                || previous.repeated != declaration.repeated
            {
                return Err(Error::AccessDenied("established field meaning changed"));
            }
        } else {
            current.fields.insert(field, declaration);
        }
        Ok(())
    }

    fn deprecate(&mut self, type_id: Id, key: &[u8]) {
        let Some(current) = self.schema.types.get_mut(&type_id) else {
            self.pending += 1;
            return;
        };
        if key.len() == 33 {
            current.deprecated = true;
        } else {
            let field = Id::from_bytes(key[33..49].try_into().expect("validated field width"));
            match current.fields.get_mut(&field) {
                Some(field) => field.deprecated = true,
                None => self.pending += 1,
            }
        }
    }
}

pub(super) fn read<S: Storage>(
    tx: &ReadTxn<'_, S>,
    entry: EntryId,
    declaration: SourceValue<'_>,
) -> Result<(Schema, usize, usize)> {
    let mut state = Materializer::new(entry, declaration)?;
    for kind in [TYPE, FIELD, DEPRECATED] {
        let prefix = meta_key(kind, entry);
        for item in tx.iter_from(&prefix) {
            let (key, _) = item?;
            if !key.starts_with(&prefix) {
                break;
            }
            let (clock, bytes) = tx
                .get_with_clock(&key)?
                .ok_or(Error::corrupt("schema revision record is missing"))?;
            state.observe(&key, clock, &bytes)?;
        }
    }
    state.schema.validate()?;
    Ok((state.schema, state.charge, state.pending))
}

pub(super) fn repair<S: Storage>(local: &LocalIndexes<'_, '_, S>, entry: EntryId) -> Result<()> {
    let mut has_facts = false;
    for kind in [TYPE, FIELD, DEPRECATED] {
        let prefix = meta_key(kind, entry);
        has_facts |= local
            .source_iter_from(&prefix)
            .next()
            .transpose()?
            .is_some_and(|(key, _)| key.starts_with(&prefix));
    }
    if !has_facts {
        return Ok(());
    }
    read_pending(local, entry).map(|_| ())
}

fn read_pending<S: Storage>(local: &LocalIndexes<'_, '_, S>, entry: EntryId) -> Result<Schema> {
    let (clock, bytes) =
        local.source_get_with_clock(&entry_key(entry))?.ok_or(Error::NotFound("entry"))?;
    let mut state = Materializer::new(entry, (clock, &bytes))?;
    for kind in [TYPE, FIELD, DEPRECATED] {
        let prefix = meta_key(kind, entry);
        for item in local.source_iter_from(&prefix) {
            let (key, _) = item?;
            if !key.starts_with(&prefix) {
                break;
            }
            let (clock, bytes) = local
                .source_get_with_clock(&key)?
                .ok_or(Error::corrupt("schema source revision record is missing"))?;
            state.observe(&key, clock, &bytes)?;
        }
    }
    state.schema.validate()?;
    Ok(state.schema)
}

fn write_fact<S: Storage>(
    tx: &mut WriteTxn<'_, S>,
    key: &[u8],
    creation: [u8; 32],
    declaration: Declaration,
) -> Result<()> {
    let expected = Fact { creation, declaration };
    if let Some(current) = tx.get(key)? {
        if fact(key, &current)? != expected {
            return Err(Error::AccessDenied("schema identity changed"));
        }
        // Equal known meaning is a no-op. Keep the original optional fields
        // and originating clock instead of re-encoding a partial projection.
        return Ok(());
    }
    let bytes = encode(&expected)?;
    tx.put(key, &bytes)?;
    Ok(())
}

pub(super) fn stage<S: Storage>(
    tx: &mut WriteTxn<'_, S>,
    entry: EntryId,
    declaration: &Schema,
) -> Result<bool> {
    if tx.get(&meta_key(RETIRED, entry))?.is_some() {
        return Err(Error::NotFound("live entry"));
    }
    let (clock, bytes) = tx.get_with_clock(&entry_key(entry))?.ok_or(Error::NotFound("entry"))?;
    if !matches!(info(&bytes)?.kind, EntryKind::ObjectDb { .. }) {
        return Err(Error::InvalidInput("entry is not an object database"));
    }
    let creation = creation_digest(entry, clock, &bytes);
    drop(bytes);
    let current = read_pending(&tx.local_indexes(), entry)?;
    let mut successor = current.clone();
    successor.establish(declaration)?;
    if successor == current {
        return Ok(false);
    }
    for (type_id, declared) in &declaration.types {
        if !current.types.contains_key(type_id) {
            write_fact(
                tx,
                &type_key(TYPE, entry, *type_id),
                creation,
                Declaration::Type { name: declared.name.clone() },
            )?;
        }
        if declared.deprecated {
            write_fact(
                tx,
                &type_key(DEPRECATED, entry, *type_id),
                creation,
                Declaration::Deprecated,
            )?;
        }
        for (field_id, field) in &declared.fields {
            stage_field(
                tx,
                entry,
                creation,
                *type_id,
                *field_id,
                field,
                current.types.get(type_id).is_some_and(|t| t.fields.contains_key(field_id)),
            )?;
        }
    }
    Ok(true)
}

fn stage_field<S: Storage>(
    tx: &mut WriteTxn<'_, S>,
    entry: EntryId,
    creation: [u8; 32],
    type_id: Id,
    field_id: Id,
    field: &FieldDeclaration,
    exists: bool,
) -> Result<()> {
    if !exists {
        write_fact(
            tx,
            &field_key(FIELD, entry, type_id, field_id),
            creation,
            Declaration::Field {
                name: field.name.clone(),
                value_type: field.value_type,
                repeated: field.repeated,
            },
        )?;
    }
    if field.deprecated {
        write_fact(
            tx,
            &field_key(DEPRECATED, entry, type_id, field_id),
            creation,
            Declaration::Deprecated,
        )?;
    }
    Ok(())
}
