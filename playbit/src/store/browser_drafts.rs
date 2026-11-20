//! Installation-local editor recovery. Drafts never authorize or publish data.
use super::object_contract::{Acl, EntryAcl, Subject, Value, ValueType};
use super::{EntryId, Error, Id, Result, catalog::record, key_value};
use record::{Codec, Mode, View};
use serde::{Deserialize, Serialize};

pub const MAX_DRAFTS: usize = 32;
pub const MAX_ACCOUNT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RECORD_BYTES: usize = 512 * 1024;
pub const MAX_ANCESTORS: usize = 64;
pub const MAX_KEY_TEXT: usize = kvdb::KEY_SIZE_MAX * 2;
pub const MAX_VALUE_TEXT: usize = key_value::MAX_BYTES * 2 + (key_value::MAX_BYTES - 1) / 4;
pub const MAX_RECORD_FIELDS: usize = 16;
pub const MAX_FIELD_TEXT: usize = 8192;
pub const MAX_VALUES: usize = 128;
pub const MAX_PRINCIPAL_TEXT: usize = 128;
const MAX_ACL_GRANTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum DraftEncoding {
    Text,
    Hex,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct BrowserDraft {
    pub id: Id,
    pub revision: u64,
    pub volume: Id,
    pub entry: Option<EntryId>,
    /// Ordered parent directories, excluding the edited entry. Navigation hints only.
    pub ancestors: Vec<EntryId>,
    pub body: BrowserDraftBody,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum BrowserDraftBody {
    Kv(KvDraft),
    Permissions(PermissionDraft),
    Record(RecordDraft),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct KvDraft {
    pub creating: bool,
    pub original_key: Option<Vec<u8>>,
    pub key_text: String,
    pub value_text: String,
    pub key_encoding: DraftEncoding,
    pub value_encoding: DraftEncoding,
    pub base: Option<KvDraftBase>,
    pub changed_during_edit: bool,
    pub uncertain: bool,
    pub read_unavailable: bool,
    pub complete_put: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct KvDraftBase {
    pub root: key_value::Root,
    pub offset: u64,
    pub total_bytes: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct PermissionDraft {
    pub base_active: EntryAcl,
    pub base_retained: Option<Acl>,
    pub active: EntryAcl,
    pub retained: Option<Acl>,
    pub adding_grant: bool,
    pub grant_input: String,
    pub grant_target: Option<Subject>,
    pub grant_service: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct RecordDraft {
    pub type_id: Id,
    pub object: Id,
    pub fields: Vec<RecordFieldDraft>,
    pub values: Vec<RecordValuesDraft>,
    pub active_values: Option<Id>,
    pub changed_during_edit: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct RecordFieldDraft {
    pub field: Id,
    pub text: String,
    pub base_values: Option<Vec<Value>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct RecordValuesDraft {
    pub field: Id,
    pub kind: ValueType,
    pub repeated: bool,
    pub base_values: Option<Vec<Value>>,
    pub values: Option<Vec<Value>>,
    pub input: String,
    pub editing: Option<u32>,
    pub after: u32,
    pub input_dirty: bool,
    pub dirty: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum BrowserDraftTarget {
    Permissions,
    Kv { original_key: Option<Vec<u8>>, creating: bool },
    Record { type_id: Id, object: Id },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct BrowserDraftSummary {
    pub id: Id,
    pub revision: u64,
    pub volume: Id,
    pub entry: Option<EntryId>,
    pub ancestors: Vec<EntryId>,
    pub target: BrowserDraftTarget,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum BrowserDraftOperation {
    List,
    Get { id: Id },
    Upsert { expected_revision: Option<u64>, draft: Box<BrowserDraft> },
    Remove { id: Id, expected_revision: u64 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum BrowserDraftResult {
    List(Vec<BrowserDraftSummary>),
    Draft { id: Id, draft: Option<Box<BrowserDraft>> },
    Stored { id: Id, revision: u64 },
    Removed { id: Id, revision: u64 },
}

#[cfg(feature = "app-host")]
impl ArchivedBrowserDraftResult {
    pub(crate) fn matches_operation(&self, operation: &BrowserDraftOperation) -> bool {
        match (self, operation) {
            (Self::List(summaries), BrowserDraftOperation::List) => {
                summaries.len() <= MAX_DRAFTS
                    && summaries.iter().all(|summary| {
                        summary.revision.to_native() != 0
                            && summary.ancestors.len() <= MAX_ANCESTORS
                    })
            }
            (Self::Draft { id, draft }, BrowserDraftOperation::Get { id: expected }) => {
                id.0 == *expected.as_bytes()
                    && draft.as_ref().is_none_or(|draft| draft.id.0 == id.0)
            }
            (Self::Stored { id, revision }, BrowserDraftOperation::Upsert { draft, .. }) => {
                id.0 == *draft.id.as_bytes() && revision.to_native() == draft.revision
            }
            (
                Self::Removed { id, revision },
                BrowserDraftOperation::Remove { id: expected, expected_revision },
            ) => id.0 == *expected.as_bytes() && revision.to_native() == *expected_revision,
            _ => false,
        }
    }
}

impl BrowserDraft {
    /// Retained DTO storage, including spare String/Vec capacity. Does not count
    /// editor entities, archives, or an independently cloned in-flight snapshot.
    pub fn retained_bytes(&self) -> Result<usize> {
        self.validate()?;
        let mut bytes = std::mem::size_of::<Self>();
        charge(&mut bytes, self.ancestors.capacity(), std::mem::size_of::<EntryId>())?;
        match &self.body {
            BrowserDraftBody::Kv(draft) => {
                charge(&mut bytes, draft.key_text.capacity(), 1)?;
                charge(&mut bytes, draft.value_text.capacity(), 1)?;
                if let Some(key) = &draft.original_key {
                    charge(&mut bytes, key.capacity(), 1)?;
                }
                if let Some(base) = &draft.base {
                    charge(&mut bytes, base.bytes.capacity(), 1)?;
                }
            }
            BrowserDraftBody::Permissions(draft) => {
                charge(&mut bytes, draft.grant_input.capacity(), 1)?;
                for active in [&draft.base_active, &draft.active] {
                    if let EntryAcl::Replace(acl) = active {
                        acl_memory(&mut bytes, acl)?;
                    }
                }
                for acl in [&draft.base_retained, &draft.retained].into_iter().flatten() {
                    acl_memory(&mut bytes, acl)?;
                }
            }
            BrowserDraftBody::Record(draft) => {
                charge(
                    &mut bytes,
                    draft.fields.capacity(),
                    std::mem::size_of::<RecordFieldDraft>(),
                )?;
                for field in &draft.fields {
                    charge(&mut bytes, field.text.capacity(), 1)?;
                    values_memory(&mut bytes, field.base_values.as_ref())?;
                }
                charge(
                    &mut bytes,
                    draft.values.capacity(),
                    std::mem::size_of::<RecordValuesDraft>(),
                )?;
                for values in &draft.values {
                    charge(&mut bytes, values.input.capacity(), 1)?;
                    values_memory(&mut bytes, values.base_values.as_ref())?;
                    values_memory(&mut bytes, values.values.as_ref())?;
                }
            }
        }
        Ok(bytes)
    }

    pub fn encoded_len(&self) -> Result<usize> {
        self.validate()?;
        let length = record::encode(self)?.len();
        if length > MAX_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        Ok(length)
    }

    pub fn summary(&self) -> BrowserDraftSummary {
        let target = match &self.body {
            BrowserDraftBody::Kv(draft) => BrowserDraftTarget::Kv {
                original_key: draft.original_key.clone(),
                creating: draft.creating,
            },
            BrowserDraftBody::Permissions(_) => BrowserDraftTarget::Permissions,
            BrowserDraftBody::Record(draft) => {
                BrowserDraftTarget::Record { type_id: draft.type_id, object: draft.object }
            }
        };
        BrowserDraftSummary {
            id: self.id,
            revision: self.revision,
            volume: self.volume,
            entry: self.entry,
            ancestors: self.ancestors.clone(),
            target,
        }
    }

    pub fn canonicalize(&mut self) {
        if let BrowserDraftBody::Permissions(draft) = &mut self.body {
            if let EntryAcl::Replace(acl) = &draft.base_active {
                draft.base_retained = Some(acl.clone());
            }
            if let EntryAcl::Replace(acl) = &draft.active {
                draft.retained = Some(acl.clone());
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.revision == 0
            || self.ancestors.len() > MAX_ANCESTORS
            || self.ancestors.iter().enumerate().any(|(index, ancestor)| {
                Some(*ancestor) == self.entry || self.ancestors[..index].contains(ancestor)
            })
        {
            return Err(Error::InvalidInput("invalid browser draft identity or ancestors"));
        }
        match &self.body {
            BrowserDraftBody::Kv(draft) => validate_kv(draft, self.entry),
            BrowserDraftBody::Permissions(draft) => validate_permissions(draft, self.entry),
            BrowserDraftBody::Record(draft) => validate_record(draft, self.entry),
        }
    }
}

fn validate_kv(draft: &KvDraft, entry: Option<EntryId>) -> Result<()> {
    if entry.is_none()
        || draft.key_text.len() > MAX_KEY_TEXT
        || draft.value_text.len() > MAX_VALUE_TEXT
        || draft
            .original_key
            .as_ref()
            .is_some_and(|key| key.is_empty() || key.len() > kvdb::KEY_SIZE_MAX)
        || (draft.creating && draft.original_key.is_some())
    {
        return Err(Error::InvalidInput("invalid key-value draft bounds"));
    }
    if let Some(base) = &draft.base
        && (base.bytes.len() > key_value::MAX_BYTES
            || base.total_bytes > key_value::MAX_STORED_BYTES as u64
            || base
                .offset
                .checked_add(base.bytes.len() as u64)
                .is_none_or(|end| end > base.total_bytes))
    {
        return Err(Error::InvalidInput("invalid key-value draft baseline"));
    }
    Ok(())
}

fn validate_permissions(draft: &PermissionDraft, entry: Option<EntryId>) -> Result<()> {
    if draft.grant_input.len() > MAX_PRINCIPAL_TEXT {
        return Err(Error::LimitExceeded);
    }
    if entry.is_none()
        && (!matches!(draft.base_active, EntryAcl::Replace(_))
            || !matches!(draft.active, EntryAcl::Replace(_)))
    {
        return Err(Error::InvalidInput("volume permission drafts require explicit ACLs"));
    }
    for active in [&draft.base_active, &draft.active] {
        if let EntryAcl::Replace(acl) = active {
            validate_acl(acl)?;
        }
    }
    for acl in [&draft.base_retained, &draft.retained].into_iter().flatten() {
        validate_acl(acl)?;
    }
    Ok(())
}

fn validate_record(draft: &RecordDraft, entry: Option<EntryId>) -> Result<()> {
    if entry.is_none()
        || draft.fields.len() > MAX_RECORD_FIELDS
        || draft.values.len() > MAX_RECORD_FIELDS
        || draft
            .active_values
            .is_some_and(|field| !draft.values.iter().any(|values| values.field == field))
    {
        return Err(Error::InvalidInput("invalid record draft bounds"));
    }
    for (index, field) in draft.fields.iter().enumerate() {
        if field.text.len() > MAX_FIELD_TEXT
            || draft.fields[..index].iter().any(|other| other.field == field.field)
        {
            return Err(Error::InvalidInput("invalid record field draft"));
        }
        validate_scalar_values(field.base_values.as_deref())?;
    }
    for (index, values) in draft.values.iter().enumerate() {
        if values.input.len() > MAX_FIELD_TEXT
            || draft.values[..index].iter().any(|other| other.field == values.field)
            || values.after as usize > MAX_VALUES
            || values
                .editing
                .is_some_and(|index| index as usize >= values.values.as_ref().map_or(0, Vec::len))
        {
            return Err(Error::InvalidInput("invalid value-list draft cursor"));
        }
        validate_values(values.base_values.as_deref())?;
        validate_values(values.values.as_deref())?;
    }
    Ok(())
}

fn validate_acl(acl: &Acl) -> Result<()> {
    if acl.grants.len() > MAX_ACL_GRANTS
        || acl.grants.iter().any(|grant| grant.permissions.len() > 4)
    {
        return Err(Error::LimitExceeded);
    }
    acl.validate()
}

fn charge(bytes: &mut usize, count: usize, width: usize) -> Result<()> {
    *bytes = count
        .checked_mul(width)
        .and_then(|next| bytes.checked_add(next))
        .ok_or(Error::LimitExceeded)?;
    Ok(())
}
fn acl_memory(bytes: &mut usize, acl: &Acl) -> Result<()> {
    charge(bytes, acl.grants.capacity(), std::mem::size_of::<super::object_contract::Grant>())?;
    for grant in &acl.grants {
        charge(
            bytes,
            grant.permissions.capacity(),
            std::mem::size_of::<super::object_contract::Permission>(),
        )?;
    }
    Ok(())
}
fn values_memory(bytes: &mut usize, values: Option<&Vec<Value>>) -> Result<()> {
    if let Some(values) = values {
        charge(bytes, values.capacity(), std::mem::size_of::<Value>())?;
        for value in values {
            match value {
                Value::Text(value) => charge(bytes, value.capacity(), 1)?,
                Value::Bytes(value) => charge(bytes, value.capacity(), 1)?,
                _ => {}
            }
        }
    }
    Ok(())
}

fn validate_values(values: Option<&[Value]>) -> Result<()> {
    let values = values.unwrap_or_default();
    if values.len() > MAX_VALUES {
        return Err(Error::LimitExceeded);
    }
    let mut bytes = 0usize;
    for value in values {
        let payload = match value {
            Value::Text(text) => text.len().checked_mul(6),
            Value::Bytes(bytes) => bytes.len().checked_mul(4),
            Value::File { .. } => Some(128),
            Value::Reference { .. } => Some(192),
            Value::Float(value) if !value.is_finite() => {
                return Err(Error::InvalidInput("nonfinite draft value"));
            }
            _ => Some(32),
        }
        .ok_or(Error::LimitExceeded)?;
        bytes = bytes
            .checked_add(payload)
            .and_then(|bytes| bytes.checked_add(32))
            .ok_or(Error::LimitExceeded)?;
        if bytes > MAX_FIELD_TEXT {
            return Err(Error::LimitExceeded);
        }
    }
    Ok(())
}

// Scalar text editors already admit 8 KiB of literal input. Preserve that full
// baseline without applying the repeated editor's conservative JSON estimate.
fn validate_scalar_values(values: Option<&[Value]>) -> Result<()> {
    let values = values.unwrap_or_default();
    if values.len() > MAX_VALUES {
        return Err(Error::LimitExceeded);
    }
    let mut bytes = 0usize;
    for value in values {
        let payload = match value {
            Value::Text(text) => text.len(),
            Value::Bytes(bytes) => bytes.len(),
            Value::File { .. } => 31,
            Value::Reference { .. } => 47,
            Value::Boolean(_) => 1,
            Value::Float(value) if !value.is_finite() => {
                return Err(Error::InvalidInput("nonfinite draft value"));
            }
            _ => 8,
        };
        bytes = bytes
            .checked_add(payload)
            .filter(|bytes| *bytes <= MAX_FIELD_TEXT)
            .ok_or(Error::LimitExceeded)?;
    }
    Ok(())
}

fn read_text(value: View<'_>, limit: usize) -> Result<String> {
    let bytes = value.body(record::TEXT)?;
    if bytes.len() > limit {
        return Err(Error::LimitExceeded);
    }
    std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| Error::corrupt("invalid draft UTF-8"))
}
fn read_values(value: View<'_>, mode: Mode, scalar: bool) -> Result<Option<Vec<Value>>> {
    if value.0 == [record::NULL] {
        return Ok(None);
    }
    if value.sequence()?.count()? > MAX_VALUES {
        return Err(Error::LimitExceeded);
    }
    let values = Vec::<Value>::read(value, mode)?;
    if scalar {
        validate_scalar_values(Some(&values))?;
    } else {
        validate_values(Some(&values))?;
    }
    Ok(Some(values))
}
fn read_acl(value: View<'_>, mode: Mode) -> Result<Acl> {
    let grants = value.fields(mode, &[1])?.get(1)?;
    if grants.sequence()?.count()? > MAX_ACL_GRANTS {
        return Err(Error::LimitExceeded);
    }
    let mut entries = grants.sequence()?;
    while let Some(grant) = entries.next_value()? {
        if grant.fields(mode, &[1, 2])?.get(2)?.sequence()?.count()? > 4 {
            return Err(Error::LimitExceeded);
        }
    }
    let acl = Acl::read(value, mode)?;
    validate_acl(&acl)?;
    Ok(acl)
}
fn read_optional_acl(value: View<'_>, mode: Mode) -> Result<Option<Acl>> {
    if value.0 == [record::NULL] { Ok(None) } else { read_acl(value, mode).map(Some) }
}
fn read_entry_acl(value: View<'_>, mode: Mode) -> Result<EntryAcl> {
    let (tag, body) = value.variant()?;
    match tag {
        1 => {
            body.fields(mode, &[])?;
            Ok(EntryAcl::Inherit)
        }
        2 => read_acl(body.fields(mode, &[1])?.get(1)?, mode).map(EntryAcl::Replace),
        _ => Err(Error::InvalidInput("unsupported draft ACL kind")),
    }
}
impl Codec for BrowserDraft {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        out.push(record::STRUCT);
        record::put(out, 1, &self.id)?;
        record::put(out, 2, &self.revision)?;
        record::put(out, 3, &self.volume)?;
        record::put(out, 4, &self.entry)?;
        record::put(out, 5, &self.ancestors)?;
        record::put(out, 6, &self.body)
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let fields = value.fields(mode, &[1, 2, 3, 4, 5, 6])?;
        if fields.get(5)?.body(record::ENTRY_IDS)?.len() > MAX_ANCESTORS * 15 {
            return Err(Error::LimitExceeded);
        }
        Ok(Self {
            id: fields.required(1, mode)?,
            revision: fields.required(2, mode)?,
            volume: fields.required(3, mode)?,
            entry: fields.required(4, mode)?,
            ancestors: fields.required(5, mode)?,
            body: fields.required(6, mode)?,
        })
    }
}
impl Codec for PermissionDraft {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        out.push(record::STRUCT);
        record::put(out, 1, &self.base_active)?;
        record::put(out, 2, &self.base_retained)?;
        record::put(out, 3, &self.active)?;
        record::put(out, 4, &self.retained)?;
        record::put(out, 5, &self.adding_grant)?;
        record::put(out, 6, &self.grant_input)?;
        record::put(out, 7, &self.grant_target)?;
        record::put(out, 8, &self.grant_service)
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let fields = value.fields(mode, &[1, 2, 3, 4, 5, 6, 7, 8])?;
        Ok(Self {
            base_active: read_entry_acl(fields.get(1)?, mode)?,
            base_retained: read_optional_acl(fields.get(2)?, mode)?,
            active: read_entry_acl(fields.get(3)?, mode)?,
            retained: read_optional_acl(fields.get(4)?, mode)?,
            adding_grant: fields.required(5, mode)?,
            grant_input: read_text(fields.get(6)?, MAX_PRINCIPAL_TEXT)?,
            grant_target: fields.required(7, mode)?,
            grant_service: fields.required(8, mode)?,
        })
    }
}
impl Codec for RecordDraft {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        out.push(record::STRUCT);
        record::put(out, 1, &self.type_id)?;
        record::put(out, 2, &self.object)?;
        record::put(out, 3, &self.fields)?;
        record::put(out, 4, &self.values)?;
        record::put(out, 5, &self.changed_during_edit)?;
        record::put(out, 6, &self.active_values)
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let fields = value.fields(mode, &[1, 2, 3, 4, 5, 6])?;
        if fields.get(3)?.sequence()?.count()? > MAX_RECORD_FIELDS
            || fields.get(4)?.sequence()?.count()? > MAX_RECORD_FIELDS
        {
            return Err(Error::LimitExceeded);
        }
        Ok(Self {
            type_id: fields.required(1, mode)?,
            object: fields.required(2, mode)?,
            fields: fields.required(3, mode)?,
            values: fields.required(4, mode)?,
            changed_during_edit: fields.required(5, mode)?,
            active_values: fields.required(6, mode)?,
        })
    }
}
impl Codec for RecordFieldDraft {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        out.push(record::STRUCT);
        record::put(out, 1, &self.field)?;
        record::put(out, 2, &self.text)?;
        record::put(out, 3, &self.base_values)
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let fields = value.fields(mode, &[1, 2, 3])?;
        Ok(Self {
            field: fields.required(1, mode)?,
            text: read_text(fields.get(2)?, MAX_FIELD_TEXT)?,
            base_values: read_values(fields.get(3)?, mode, true)?,
        })
    }
}

impl Codec for DraftEncoding {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        record::variant(
            out,
            match self {
                Self::Text => 1,
                Self::Hex => 2,
            },
        );
        Ok(())
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let (tag, body) = value.variant()?;
        body.fields(mode, &[])?;
        match tag {
            1 => Ok(Self::Text),
            2 => Ok(Self::Hex),
            _ => Err(Error::InvalidInput("unsupported draft encoding")),
        }
    }
}

impl Codec for BrowserDraftBody {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Kv(value) => {
                record::variant(out, 1);
                record::put(out, 1, value)
            }
            Self::Permissions(value) => {
                record::variant(out, 2);
                record::put(out, 1, value)
            }
            Self::Record(value) => {
                record::variant(out, 3);
                record::put(out, 1, value)
            }
        }
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let (tag, body) = value.variant()?;
        let fields = body.fields(mode, &[1])?;
        match tag {
            1 => Ok(Self::Kv(fields.required(1, mode)?)),
            2 => Ok(Self::Permissions(fields.required(1, mode)?)),
            3 => Ok(Self::Record(fields.required(1, mode)?)),
            _ => Err(Error::InvalidInput("unsupported browser draft kind")),
        }
    }
}

// Draft-only BYTES fields avoid the generic sequence's 4,096-item bound.
fn put_bytes(out: &mut Vec<u8>, tag: u16, bytes: Option<&[u8]>) -> Result<()> {
    let Some(bytes) = bytes else {
        return record::put_raw(out, tag, &[record::NULL]);
    };
    out.extend_from_slice(&(tag | record::REQUIRED).to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(bytes.len() + 1).map_err(|_| Error::LimitExceeded)?.to_le_bytes(),
    );
    record::raw(out, record::BYTES, bytes)
}
fn read_bytes(value: View<'_>, limit: usize) -> Result<Vec<u8>> {
    let bytes = value.body(record::BYTES)?;
    if bytes.len() > limit {
        return Err(Error::LimitExceeded);
    }
    Ok(bytes.to_vec())
}
impl Codec for KvDraftBase {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        out.push(record::STRUCT);
        record::put(out, 1, &u64::from(self.root.level))?;
        record::put(out, 2, &self.root.digest)?;
        record::put(out, 3, &self.offset)?;
        record::put(out, 4, &self.total_bytes)?;
        put_bytes(out, 5, Some(&self.bytes))
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let fields = value.fields(mode, &[1, 2, 3, 4, 5])?;
        let level = u8::try_from(fields.required::<u64>(1, mode)?)
            .map_err(|_| Error::InvalidInput("invalid draft root level"))?;
        Ok(Self {
            root: key_value::Root { level, digest: fields.required(2, mode)? },
            offset: fields.required(3, mode)?,
            total_bytes: fields.required(4, mode)?,
            bytes: read_bytes(fields.get(5)?, key_value::MAX_BYTES)?,
        })
    }
}
impl Codec for KvDraft {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        out.push(record::STRUCT);
        record::put(out, 1, &self.creating)?;
        put_bytes(out, 2, self.original_key.as_deref())?;
        record::put(out, 3, &self.key_text)?;
        record::put(out, 4, &self.value_text)?;
        record::put(out, 5, &self.key_encoding)?;
        record::put(out, 6, &self.value_encoding)?;
        record::put(out, 7, &self.base)?;
        record::put(out, 8, &self.changed_during_edit)?;
        record::put(out, 9, &self.uncertain)?;
        record::put(out, 10, &self.read_unavailable)?;
        record::put(out, 11, &self.complete_put)
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let fields = value.fields(mode, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11])?;
        let original = fields.get(2)?;
        Ok(Self {
            creating: fields.required(1, mode)?,
            original_key: if original.0 == [record::NULL] {
                None
            } else {
                Some(read_bytes(original, kvdb::KEY_SIZE_MAX)?)
            },
            key_text: read_text(fields.get(3)?, MAX_KEY_TEXT)?,
            value_text: read_text(fields.get(4)?, MAX_VALUE_TEXT)?,
            key_encoding: fields.required(5, mode)?,
            value_encoding: fields.required(6, mode)?,
            base: fields.required(7, mode)?,
            changed_during_edit: fields.required(8, mode)?,
            uncertain: fields.required(9, mode)?,
            read_unavailable: fields.required(10, mode)?,
            complete_put: fields.required(11, mode)?,
        })
    }
}
impl Codec for RecordValuesDraft {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        out.push(record::STRUCT);
        record::put(out, 1, &self.field)?;
        record::put(out, 2, &self.kind)?;
        record::put(out, 3, &self.repeated)?;
        record::put(out, 4, &self.base_values)?;
        record::put(out, 5, &self.values)?;
        record::put(out, 6, &self.input)?;
        record::put(out, 7, &self.editing.map(u64::from))?;
        record::put(out, 8, &u64::from(self.after))?;
        record::put(out, 9, &self.input_dirty)?;
        record::put(out, 10, &self.dirty)
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let fields = value.fields(mode, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10])?;
        let editing = fields
            .required::<Option<u64>>(7, mode)?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| Error::LimitExceeded)?;
        Ok(Self {
            field: fields.required(1, mode)?,
            kind: fields.required(2, mode)?,
            repeated: fields.required(3, mode)?,
            base_values: read_values(fields.get(4)?, mode, false)?,
            values: read_values(fields.get(5)?, mode, false)?,
            input: read_text(fields.get(6)?, MAX_FIELD_TEXT)?,
            editing,
            after: u32::try_from(fields.required::<u64>(8, mode)?)
                .map_err(|_| Error::LimitExceeded)?,
            input_dirty: fields.required(9, mode)?,
            dirty: fields.required(10, mode)?,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "Tests assert bounded draft recovery fixtures.")]
    use super::*;
    use crate::store::object_contract::Principal;

    #[test]
    fn principal_input_recovers_exactly_and_is_bounded_before_text_ownership() {
        // Preserve unfinished raw text and all add-grant state; the limit counts UTF-8 bytes.
        let mut spare_capacity = String::with_capacity(MAX_PRINCIPAL_TEXT * 2);
        spare_capacity.push_str("unfinished 0 O I\n");
        for (input, admitted) in [
            (spare_capacity, true),
            ("x".repeat(MAX_PRINCIPAL_TEXT), true),
            ("é".repeat(MAX_PRINCIPAL_TEXT / 2), true),
            ("x".repeat(MAX_PRINCIPAL_TEXT + 1), false),
            ("é".repeat(MAX_PRINCIPAL_TEXT / 2 + 1), false),
        ] {
            let capacity = input.capacity();
            let draft = BrowserDraft {
                id: Id::from_bytes([1; 16]),
                revision: 1,
                volume: Id::from_bytes([2; 16]),
                entry: Some(EntryId::from_bytes([3; 15]).unwrap()),
                ancestors: vec![],
                body: BrowserDraftBody::Permissions(PermissionDraft {
                    base_active: EntryAcl::Inherit,
                    base_retained: None,
                    active: EntryAcl::Inherit,
                    retained: None,
                    adding_grant: true,
                    grant_input: input,
                    grant_target: Some(Subject::Principal(Principal::Service(Id::from_bytes(
                        [4; 16],
                    )))),
                    grant_service: true,
                }),
            };
            let wire = record::encode(&draft).unwrap();
            if admitted {
                draft.validate().unwrap();
                assert_eq!(
                    draft.retained_bytes().unwrap(),
                    std::mem::size_of::<BrowserDraft>() + capacity
                );
                assert_eq!(draft.encoded_len().unwrap(), wire.len());
                assert_eq!(record::decode::<BrowserDraft>(&wire).unwrap(), draft);
                assert_eq!(record::read::<BrowserDraft>(&wire).unwrap(), draft);
            } else {
                assert!(matches!(draft.validate(), Err(Error::LimitExceeded)));
                assert!(matches!(draft.retained_bytes(), Err(Error::LimitExceeded)));
                assert!(matches!(draft.encoded_len(), Err(Error::LimitExceeded)));
                assert!(matches!(record::decode::<BrowserDraft>(&wire), Err(Error::LimitExceeded)));
                assert!(matches!(record::read::<BrowserDraft>(&wire), Err(Error::LimitExceeded)));
            }
        }
    }
}
