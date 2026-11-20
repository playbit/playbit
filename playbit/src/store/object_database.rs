//! Objects owned by one named database. The immutable creation fact includes
//! initial fields; delivery order never exposes a half-created object. Entry
//! authorization and the volume operation boundary are the caller's responsibility.
use super::{
    Error, Id, Result,
    catalog::record::{self, Codec, Mode, View, encode},
    object_contract::{EditStamp, FieldCell, FieldEdit, ObjectFields, Schema, Value},
    query::{self, Limits, Scratch},
};
use kvdb::{Clock, ReadTxn, Storage, WriteTxn};
use playbit_api::abi::syscall::{MAX_FIELDS, MAX_VALUES};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const MAX_STORED_RECORD_BYTES: usize = 4 * 1024 * 1024;
const MAX_CHANGED_OBJECTS: usize = 64;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Creation {
    creator: Id,
    stamp: EditStamp,
    record: ObjectFields,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Field {
    creation: [u8; 32],
    cell: FieldCell,
}

record::fields!(Creation { 1 => creator, 2 => stamp, 3 => record });
record::fields!(Field { 1 => creation, 2 => cell });

pub fn creation_key(object: Id) -> [u8; 23] {
    // Queries scan these keys in binary ID order, independent of display text.
    let mut key = [0; 23];
    key[..7].copy_from_slice(b"object/");
    key[7..].copy_from_slice(object.as_bytes());
    key
}
pub fn field_key(object: Id, field: Id) -> String {
    format!("field/{object}/{field}")
}
fn retired_key(object: Id) -> String {
    format!("retired/{object}")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Key {
    Creation(Id),
    Field(Id, Id),
    Retired(Id),
}
impl Key {
    fn object(self) -> Id {
        match self {
            Self::Creation(id) | Self::Field(id, _) | Self::Retired(id) => id,
        }
    }
    fn parse(key: &[u8]) -> Result<Self> {
        if let Some(id) = key.strip_prefix(b"object/") {
            return Ok(Self::Creation(Id::from_bytes(
                id.try_into()
                    .map_err(|_| Error::corrupt("object creation key has the wrong ID width"))?,
            )));
        }
        let key = std::str::from_utf8(key)
            .map_err(|_| Error::corrupt("object database key is not UTF-8"))?;
        if let Some(id) = key.strip_prefix("retired/") {
            return Ok(Self::Retired(Id::parse(id)?));
        }
        if let Some(rest) = key.strip_prefix("field/")
            && let Some((object, field)) = rest.split_once('/')
        {
            return Ok(Self::Field(Id::parse(object)?, Id::parse(field)?));
        }
        Err(Error::corrupt("object database key has an unknown shape"))
    }
}

fn anchor(raw: &[u8], clock: Clock) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"playbit.object.creation\0");
    hash.update(&clock.as_u64().to_le_bytes());
    hash.update(raw);
    *hash.finalize().as_bytes()
}

fn parse_creation(raw: &[u8], selected: Option<&[Id]>, scratch: &mut Scratch) -> Result<Creation> {
    if raw.len() > MAX_STORED_RECORD_BYTES {
        return Err(Error::LimitExceeded);
    }
    scratch.raw(raw.len(), 0)?;
    let header = View::record(raw)?.fields(Mode::Write, &[1, 2, 3])?;
    let object = header.get(3)?.fields(Mode::Write, &[1, 2, 3])?;
    let mut fields = BTreeMap::new();
    let mut count = 0;
    let mut values = 0;
    let mut previous = None;
    let mut tail = object.get(3)?.body(record::MAP)?;
    while !tail.is_empty() {
        count += 1;
        if count > MAX_FIELDS as usize {
            return Err(Error::LimitExceeded);
        }
        let id =
            Id::from_bytes(record::take(tail, 0, 16)?.try_into().expect("sixteen-byte field ID"));
        if previous.is_some_and(|old| id <= old) {
            return Err(Error::corrupt("unordered object fields"));
        }
        previous = Some(id);
        let length =
            u32::from_le_bytes(record::take(tail, 16, 4)?.try_into().expect("four-byte length"))
                as usize;
        let cell = View(record::take(tail, 20, length)?);
        if selected.is_none_or(|fields| fields.contains(&id)) {
            fields.insert(id, query::decode_record_cell(cell, scratch, &mut values)?);
        }
        tail = &tail[20 + length..];
    }
    Ok(Creation {
        creator: header.required(1, Mode::Write)?,
        stamp: header.required(2, Mode::Write)?,
        record: ObjectFields {
            object_id: object.required(1, Mode::Write)?,
            type_id: object.required(2, Mode::Write)?,
            fields,
        },
    })
}

fn validate_creation(raw: &[u8], object: Id, clock: Clock) -> Result<Creation> {
    let mut scratch = Limits::ABI.start(1, MAX_FIELDS as usize)?;
    let creation = parse_creation(raw, None, &mut scratch)?;
    if creation.record.object_id != object
        || creation.stamp.clock != clock.as_u64()
        || creation.stamp.clock == 0
        || creation.stamp.device_id != creation.creator
        || creation.record.fields.values().any(|cell| cell.stamp != creation.stamp)
    {
        return Err(Error::AccessDenied("invalid immutable object creation"));
    }
    query::validate_record(&creation.record, Limits::ABI)?;
    Ok(creation)
}

fn parse_field(raw: &[u8], clock: Clock, scratch: &mut Scratch) -> Result<Field> {
    if raw.len() > MAX_STORED_RECORD_BYTES {
        return Err(Error::LimitExceeded);
    }
    scratch.raw(raw.len(), 0)?;
    let header = View::record(raw)?.fields(Mode::Write, &[1, 2])?;
    let cell = query::decode_record_cell(header.get(2)?, scratch, &mut 0)?;
    let field = Field { creation: header.required(1, Mode::Write)?, cell };
    if field.cell.stamp.clock != clock.as_u64() || clock == Clock::ZERO {
        return Err(Error::AccessDenied("invalid object field origin"));
    }
    Ok(field)
}

/// Pin this reader under the same operation boundary as its index/schema reader.
pub struct Snapshot<'db, S: Storage> {
    transaction: ReadTxn<'db, S>,
}
impl<'db, S: Storage> Snapshot<'db, S> {
    pub fn new(transaction: ReadTxn<'db, S>) -> Self {
        Self { transaction }
    }

    fn raw_record(&self, key: &[u8], scratch: &Scratch) -> Result<Option<(Clock, Vec<u8>)>> {
        use kvdb::storage::ValueRead;
        let Some(reader) = self.transaction.value_reader(key)? else { return Ok(None) };
        let length = reader.len();
        if length > MAX_STORED_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        scratch.raw(length, 0)?;
        let clock = reader.clock().ok_or(Error::corrupt("object record has no clock"))?;
        let mut raw = vec![0; length];
        for (index, part) in raw.chunks_mut(64 * 1024).enumerate() {
            reader.read_exact_at(index * 64 * 1024, part)?;
        }
        Ok(Some((clock, raw)))
    }

    pub fn read(&self, object: Id, selected: Option<&[Id]>) -> Result<ObjectFields> {
        if let Some(fields) = selected {
            query::validate_projection(fields, 1)?;
        }
        let mut scratch = Limits::ABI.start(1, MAX_FIELDS as usize)?;
        self.read_with_scratch(object, selected, &mut scratch)
    }

    /// Replacement edits need the stable identity and type, not a projection
    /// of previous field values. Those values remain in their independent keys.
    pub fn edit_record(&self, object: Id) -> Result<ObjectFields> {
        self.read_with_scratch(object, Some(&[]), &mut Limits::ABI.start(1, 0)?)
    }

    fn read_with_scratch(
        &self,
        object: Id,
        selected: Option<&[Id]>,
        scratch: &mut Scratch,
    ) -> Result<ObjectFields> {
        if let Some(value) = self.transaction.get(retired_key(object).as_bytes())? {
            if value.as_ref() != [1] {
                return Err(Error::corrupt("object retirement marker must be one"));
            }
            return Err(Error::NotFound("retired object"));
        }
        let (clock, raw) = self
            .raw_record(creation_key(object).as_slice(), scratch)?
            .ok_or(Error::NotFound("object"))?;
        let binding = anchor(&raw, clock);
        let mut record = parse_creation(&raw, selected, scratch)?.record;
        if record.object_id != object {
            return Err(Error::corrupt("object creation record ID differs from its key"));
        }
        drop(raw);
        self.overlay(&mut record, clock, binding, selected, scratch)?;
        query::validate_record(&record, Limits::ABI)?;
        Ok(record)
    }

    fn overlay(
        &self,
        record: &mut ObjectFields,
        birth: Clock,
        binding: [u8; 32],
        selected: Option<&[Id]>,
        scratch: &mut Scratch,
    ) -> Result<()> {
        let prefix = format!("field/{}/", record.object_id);
        let mut count = 0;
        let mut next = self.transaction.next_source_key(prefix.as_bytes(), false)?;
        while let Some(key) = next {
            next = self.transaction.next_source_key(&key, true)?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            count += 1;
            if count > MAX_FIELDS as usize {
                return Err(Error::LimitExceeded);
            }
            let Key::Field(_, field_id) = Key::parse(&key)? else {
                return Err(Error::corrupt("expected an object field key"));
            };
            if selected.is_some_and(|fields| !fields.contains(&field_id)) {
                continue;
            }
            let (clock, raw) = self
                .raw_record(&key, scratch)?
                .ok_or(Error::corrupt("object field record is missing"))?;
            let field = parse_field(&raw, clock, scratch)?;
            if field.creation != binding || clock <= birth {
                return Err(Error::AccessDenied("object field creation mismatch"));
            }
            if record
                .fields
                .get(&field_id)
                .is_some_and(|old| old.stamp == field.cell.stamp && old.values != field.cell.values)
            {
                return Err(Error::corrupt("object field stamp has conflicting values"));
            }
            if record.fields.get(&field_id).is_none_or(|old| old.stamp < field.cell.stamp) {
                record.fields.insert(field_id, field.cell);
            }
        }
        Ok(())
    }

    /// Scan a bounded number of creation facts. The resume ID is the last scanned
    /// creation, including retired and other-type records, so sparse queries progress.
    pub fn page(
        &self,
        type_id: Id,
        after: Option<Id>,
        limit: usize,
        fields: Option<&[Id]>,
    ) -> Result<super::ObjectPage> {
        self.page_with_limits(type_id, after, limit, fields, Limits::ABI)
    }

    /// Constrained hosts may lower resource ceilings without changing authority.
    pub fn page_with_limits(
        &self,
        type_id: Id,
        after: Option<Id>,
        limit: usize,
        fields: Option<&[Id]>,
        limits: Limits,
    ) -> Result<super::ObjectPage> {
        query::validate_projection(fields.unwrap_or(&[]), limit)?;
        let start = after.map(creation_key);
        let start = start.as_ref().map_or(b"object/".as_slice(), |key| key.as_slice());
        let mut objects = Vec::with_capacity(limit);
        let mut last = None;
        let mut scanned = 0;
        let mut bytes = 5usize;
        let mut scratch = limits.start(limit, fields.map_or(MAX_FIELDS as usize, <[Id]>::len))?;
        let mut key = self.transaction.next_source_key(start, after.is_some())?;
        while let Some(current) = key {
            key = self.transaction.next_source_key(&current, true)?;
            let key = current;
            if !key.starts_with(b"object/") {
                break;
            }
            let Key::Creation(id) = Key::parse(&key)? else {
                return Err(Error::corrupt("expected an object creation key"));
            };
            if after == Some(id) {
                continue;
            }
            if scanned == 64 || objects.len() == limit {
                return Ok(super::ObjectPage { objects, next: last });
            }
            let Some((_, raw)) = self.raw_record(&key, &scratch)? else { continue };
            let header = View::record(&raw)?.fields(Mode::Write, &[1, 2, 3])?;
            let object = header.get(3)?.fields(Mode::Write, &[1, 2, 3])?;
            let matches = object.required::<Id>(2, Mode::Write)? == type_id;
            drop(raw);
            if matches {
                match self.read_with_scratch(id, fields, &mut scratch) {
                    Ok(record) => {
                        let length = query::record_bytes(&record)?;
                        if bytes + length + 16 > limits.output_bytes {
                            if objects.is_empty() {
                                return Err(Error::LimitExceeded);
                            }
                            return Ok(super::ObjectPage { objects, next: last });
                        }
                        bytes += length;
                        objects.push(record);
                    }
                    Err(Error::NotFound(_)) => {}
                    Err(Error::LimitExceeded) if !objects.is_empty() => {
                        return Ok(super::ObjectPage { objects, next: last });
                    }
                    Err(error) => return Err(error),
                }
            }
            last = Some(id);
            scanned += 1;
        }
        Ok(super::ObjectPage { objects, next: None })
    }
}

/// Stage the entire initial record as one immutable authoritative key.
pub fn create(
    tx: &mut WriteTxn<'_, impl Storage>,
    schema: &Schema,
    object: Id,
    type_id: Id,
    account: Id,
    fields: &BTreeMap<Id, Option<Vec<Value>>>,
) -> Result<ObjectFields> {
    if tx.get(creation_key(object).as_slice())?.is_some()
        || tx.get(retired_key(object).as_bytes())?.is_some()
    {
        return Err(Error::AlreadyExists("object"));
    }
    let (record, stamp) = prepare_creation(schema, object, type_id, account, tx.clock(), fields)?;
    let raw = encode(&Creation { creator: account, stamp, record: record.clone() })?;
    validate_creation(&raw, object, tx.clock())?;
    tx.put(creation_key(object).as_slice(), &raw)?;
    Ok(record)
}

/// Shared admission for object creation and seed declarations. It validates the
/// same field shapes and query budgets without opening or modifying storage.
pub(crate) fn prepare_creation(
    schema: &Schema,
    object: Id,
    type_id: Id,
    account: Id,
    clock: Clock,
    fields: &BTreeMap<Id, Option<Vec<Value>>>,
) -> Result<(ObjectFields, EditStamp)> {
    if !schema.types.contains_key(&type_id) {
        return Err(Error::NotFound("object type"));
    }
    validate_input(fields)?;
    let stamp = EditStamp::new(clock, account)?;
    let mut record = ObjectFields::new(object, type_id);
    for (field_id, values) in fields {
        record.apply(
            schema,
            &FieldEdit {
                object_id: object,
                type_id,
                field_id: *field_id,
                stamp,
                values: values.clone(),
            },
        )?;
    }
    query::validate_record(&record, Limits::ABI)?;
    Ok((record, stamp))
}

fn validate_input(fields: &BTreeMap<Id, Option<Vec<Value>>>) -> Result<()> {
    if fields.len() > MAX_FIELDS as usize {
        return Err(Error::LimitExceeded);
    }
    let mut count = 0usize;
    let mut bytes = 0usize;
    for items in fields.values().flatten() {
        count = count.checked_add(items.len()).ok_or(Error::LimitExceeded)?;
        if count > MAX_VALUES as usize {
            return Err(Error::LimitExceeded);
        }
        for value in items {
            let length = match value {
                Value::Text(text) => text.len(),
                Value::Bytes(body) => body.len(),
                _ => 64,
            };
            bytes = bytes.checked_add(length).ok_or(Error::LimitExceeded)?;
            if bytes > Limits::ABI.output_bytes {
                return Err(Error::LimitExceeded);
            }
        }
    }
    Ok(())
}

fn initial_cell(raw: &[u8], field: Id) -> Result<Option<View<'_>>> {
    let creation = View::record(raw)?.fields(Mode::Write, &[1, 2, 3])?;
    let object = creation.get(3)?.fields(Mode::Write, &[1, 2, 3])?;
    let mut tail = object.get(3)?.body(record::MAP)?;
    while !tail.is_empty() {
        let id = record::take(tail, 0, 16)?;
        let length =
            u32::from_le_bytes(record::take(tail, 16, 4)?.try_into().expect("four-byte length"))
                as usize;
        let cell = View(record::take(tail, 20, length)?);
        if id == field.as_bytes() {
            return Ok(Some(cell));
        }
        tail = &tail[20 + length..];
    }
    Ok(None)
}

pub fn edit(
    tx: &mut WriteTxn<'_, impl Storage>,
    schema: &Schema,
    mut record: ObjectFields,
    account: Id,
    fields: &BTreeMap<Id, Option<Vec<Value>>>,
) -> Result<ObjectFields> {
    validate_input(fields)?;
    if tx.get(retired_key(record.object_id).as_bytes())?.is_some() {
        return Err(Error::NotFound("retired object"));
    }
    let (birth, raw) = tx
        .get_with_clock(creation_key(record.object_id).as_slice())?
        .ok_or(Error::NotFound("object"))?;
    let binding = anchor(&raw, birth);
    let birth_bytes = raw.to_vec();
    drop(raw);
    if tx.clock() <= birth {
        return Err(Error::corrupt("object mutation clock does not follow its creation"));
    }
    let stamp = EditStamp::new(tx.clock(), account)?;
    for (field_id, values) in fields {
        record.apply(
            schema,
            &FieldEdit {
                object_id: record.object_id,
                type_id: record.type_id,
                field_id: *field_id,
                stamp,
                values: values.clone(),
            },
        )?;
    }
    query::validate_record(&record, Limits::ABI)?;
    for field_id in fields.keys() {
        let key = field_key(record.object_id, *field_id);
        let next = Field { creation: binding, cell: record.fields[field_id].clone() };
        let bytes = match tx.get(key.as_bytes())? {
            Some(previous) => record::rewrite(&previous, &next)?,
            None => {
                let mut bytes = Vec::from(record::MAGIC);
                bytes.push(record::STRUCT);
                record::put(&mut bytes, 1, &binding)?;
                let cell = match initial_cell(&birth_bytes, *field_id)? {
                    Some(previous) => record::rewrite_value(previous, &next.cell)?,
                    None => {
                        let mut bytes = Vec::new();
                        next.cell.write(&mut bytes)?;
                        bytes
                    }
                };
                record::put_raw(&mut bytes, 2, &cell)?;
                bytes
            }
        };
        tx.put(key.as_bytes(), &bytes)?;
    }
    // Callers may supply only identity/type to avoid reading old field values.
    // The complete pending object must still fit the read and replication limits.
    validate_pending(tx, &BTreeSet::from([record.object_id]))?;
    Ok(record)
}

pub fn retire(tx: &mut WriteTxn<'_, impl Storage>, object: Id) -> Result<()> {
    if tx.get(creation_key(object).as_slice())?.is_none() {
        return Err(Error::NotFound("object"));
    }
    tx.put(retired_key(object).as_bytes(), &[1])?;
    Ok(())
}

/// Validate accepted source records before conflict resolution. Append authorizes
/// a complete birth only; incoming values never grant their own writer authority.
pub fn validate_source(
    delta: &kvdb::sync::Delta<'_>,
    writer: Option<Id>,
    append_only: bool,
) -> Result<Id> {
    use kvdb::sync::EntState;
    let key = Key::parse(delta.key)?;
    let EntState::Present { value, clock } = delta.remote else {
        return Err(Error::AccessDenied("object source deletion"));
    };
    if clock == Clock::ZERO {
        return Err(Error::AccessDenied("zero object source origin"));
    }
    if append_only && !matches!(key, Key::Creation(_)) {
        return Err(Error::AccessDenied("append requires a new complete object"));
    }
    match key {
        Key::Creation(id) => {
            let creation = validate_creation(value, id, clock)?;
            if writer.is_some_and(|writer| writer != creation.creator) {
                return Err(Error::AccessDenied("object creator provenance"));
            }
            if let EntState::Present { value: old, clock: origin } = delta.local
                && (old != value || origin != clock)
            {
                return Err(Error::AccessDenied("object creation is immutable"));
            }
            if matches!(delta.local, EntState::Deleted { .. }) {
                return Err(Error::AccessDenied("object creation cannot replace a tombstone"));
            }
        }
        Key::Field(_, _) => {
            let field = parse_field(value, clock, &mut Limits::ABI.start(1, 1)?)?;
            if writer.is_some_and(|writer| writer != field.cell.stamp.device_id) {
                return Err(Error::AccessDenied("object field provenance"));
            }
            if let EntState::Present { value: old, clock: origin } = delta.local {
                let old = parse_field(old, origin, &mut Limits::ABI.start(1, 1)?)?;
                if old.cell.stamp == field.cell.stamp && old.cell.values != field.cell.values {
                    return Err(Error::AccessDenied("object edit identity has conflicting values"));
                }
            }
        }
        Key::Retired(_) if value != [1] => {
            return Err(Error::AccessDenied("object retirement is permanent"));
        }
        Key::Retired(_) => {}
    }
    Ok(key.object())
}

/// Complete dependency checks against the pending writer after reordered receipt.
/// Missing creation stays pending. Invalid bindings abort every source write.
pub fn validate_pending(tx: &WriteTxn<'_, impl Storage>, objects: &BTreeSet<Id>) -> Result<()> {
    if objects.len() > MAX_CHANGED_OBJECTS {
        return Err(Error::LimitExceeded);
    }
    for object in objects {
        let Some((birth, raw)) = tx.get_with_clock(creation_key(*object).as_slice())? else {
            continue;
        };
        let binding = anchor(&raw, birth);
        let mut creation = validate_creation(&raw, *object, birth)?;
        let prefix = format!("field/{object}/");
        let mut ids: BTreeSet<Id> = creation.record.fields.keys().copied().collect();
        for item in tx.iter_from(prefix.as_bytes()) {
            let (key, _) = item?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            let Key::Field(_, field_id) = Key::parse(&key)? else {
                return Err(Error::corrupt("expected an object field key"));
            };
            ids.insert(field_id);
            if ids.len() > MAX_FIELDS as usize {
                return Err(Error::LimitExceeded);
            }
            let (clock, raw) =
                tx.get_with_clock(&key)?.ok_or(Error::corrupt("object field record is missing"))?;
            let field = parse_field(&raw, clock, &mut Limits::ABI.start(1, 1)?)?;
            if field.creation != binding || clock <= birth {
                return Err(Error::AccessDenied("object field creation mismatch"));
            }
            creation.record.fields.insert(field_id, field.cell);
        }
        query::validate_record(&creation.record, Limits::ABI)?;
    }
    Ok(())
}

/// Full snapshots validate one object at a time. Source size does not multiply
/// the fixed field/materialization allowance or require an in-memory catalog.
pub fn validate_all(tx: &WriteTxn<'_, impl Storage>) -> Result<()> {
    for item in tx.iter_from(b"object/") {
        let (key, _) = item?;
        if !key.starts_with(b"object/") {
            break;
        }
        let Key::Creation(object) = Key::parse(&key)? else {
            return Err(Error::corrupt("expected an object creation key"));
        };
        validate_pending(tx, &BTreeSet::from([object]))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
