//! Evolving schema, field-merge, and runtime permission rules.
//!
//! These are contract primitives, not an authenticated storage or sync endpoint.
//! Callers must authenticate principals and authorize edits before applying them.
//! Metadata admission bounds schema allocation before decoding; field edits are
//! bounded by their transport. These primitives never scan the durable database.

use std::collections::{BTreeMap, BTreeSet};

use kvdb::Clock;
use serde::{Deserialize, Serialize};

use crate::store::{Error, Id, Result};

/// An account identity, independent of its devices and transport connections.
/// A deserialized principal is a claim; authentication must verify its account kind.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum Principal {
    Human(Id),
    Service(Id),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum Permission {
    Read,
    Write,
    Append,
    ManagePermissions,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", content = "principal", rename_all = "snake_case")]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum Subject {
    Public,
    Principal(Principal),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Grant {
    pub subject: Subject,
    pub permissions: Vec<Permission>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Acl {
    pub grants: Vec<Grant>,
}

impl Acl {
    // fuzz: store_contract
    pub fn validate(&self) -> Result<()> {
        let mut subjects = BTreeSet::new();
        for grant in &self.grants {
            if !subjects.insert(&grant.subject) || grant.permissions.is_empty() {
                return Err(Error::InvalidInput(
                    "ACL grants require distinct subjects and permissions",
                ));
            }
            let mut permissions = BTreeSet::new();
            for permission in &grant.permissions {
                if !permissions.insert(*permission) {
                    return Err(Error::InvalidInput("ACL permissions cannot be repeated"));
                }
                if grant.subject == Subject::Public && *permission == Permission::ManagePermissions
                {
                    return Err(Error::InvalidInput("public grants cannot manage permissions"));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn checked(&self) -> Result<CheckedAcl<'_>> {
        self.validate()?;
        Ok(CheckedAcl(self))
    }
    pub fn permits(&self, principal: Option<Principal>, permission: Permission) -> bool {
        self.checked().is_ok_and(|acl| acl.permits(principal, permission))
    }
}
pub(crate) struct CheckedAcl<'a>(&'a Acl);
impl CheckedAcl<'_> {
    pub(crate) fn permits(&self, principal: Option<Principal>, permission: Permission) -> bool {
        if principal.is_none() && permission != Permission::Read {
            return false;
        }
        self.0.grants.iter().any(|grant| {
            let matches = match grant.subject {
                Subject::Public => true,
                Subject::Principal(subject) => Some(subject) == principal,
            };
            matches && grant.permissions.contains(&permission)
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "acl", rename_all = "snake_case")]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum EntryAcl {
    #[default]
    Inherit,
    Replace(Acl),
}

impl EntryAcl {
    pub fn effective<'a>(&'a self, volume_acl: &'a Acl) -> &'a Acl {
        match self {
            Self::Inherit => volume_acl,
            Self::Replace(acl) => acl,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Schema {
    pub types: BTreeMap<Id, TypeDeclaration>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct TypeDeclaration {
    pub name: String,
    pub deprecated: bool,
    pub fields: BTreeMap<Id, FieldDeclaration>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct FieldDeclaration {
    pub name: String,
    pub value_type: ValueType,
    pub repeated: bool,
    pub deprecated: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "type_id", rename_all = "snake_case")]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum ValueType {
    Text,
    Boolean,
    Integer,
    Float,
    Bytes,
    File,
    Reference(Id),
}

impl Schema {
    // fuzz: store_contract
    pub fn validate(&self) -> Result<()> {
        let mut identities = BTreeSet::new();
        for (id, declaration) in &self.types {
            require_name(&declaration.name)?;
            if !identities.insert(*id) {
                return Err(Error::InvalidInput("object type identity is reused"));
            }
            for (field_id, field) in &declaration.fields {
                require_name(&field.name)?;
                if !identities.insert(*field_id) {
                    return Err(Error::InvalidInput("object field identity is reused"));
                }
            }
        }
        Ok(())
    }

    /// Names are presentation. Concurrent declarations with distinct IDs stay
    /// usable; callers must resolve ambiguity before selecting an item by name.
    pub fn type_by_name(&self, name: &str) -> Result<Id> {
        let mut matches = self.types.iter().filter(|(_, value)| value.name == name);
        let id = *matches.next().ok_or(Error::NotFound("object type name"))?.0;
        if matches.next().is_some() {
            return Err(Error::AlreadyExists("ambiguous object type name"));
        }
        Ok(id)
    }

    pub fn field_by_name(&self, type_id: Id, name: &str) -> Result<Id> {
        let fields = &self.types.get(&type_id).ok_or(Error::NotFound("object type"))?.fields;
        let mut matches = fields.iter().filter(|(_, value)| value.name == name);
        let id = *matches.next().ok_or(Error::NotFound("object field name"))?.0;
        if matches.next().is_some() {
            return Err(Error::AlreadyExists("ambiguous object field name"));
        }
        Ok(id)
    }

    /// Add a branch/app declaration without deleting fields it does not know.
    /// Established names, identities, and value types are immutable. Deprecation
    /// is monotonic and does not disable reads or writes by older apps.
    pub fn establish(&mut self, declaration: &Schema) -> Result<()> {
        self.validate()?;
        let mut next = self.clone();
        for (id, declared) in &declaration.types {
            let Some(established) = next.types.get_mut(id) else {
                next.types.insert(*id, declared.clone());
                continue;
            };
            if established.name != declared.name {
                return Err(Error::InvalidInput("object types cannot be renamed"));
            }
            established.deprecated |= declared.deprecated;
            for (field_id, field) in &declared.fields {
                if let Some(previous) = established.fields.get_mut(field_id) {
                    if previous.name != field.name
                        || previous.value_type != field.value_type
                        || previous.repeated != field.repeated
                    {
                        return Err(Error::InvalidInput("object field meaning cannot change"));
                    }
                    previous.deprecated |= field.deprecated;
                } else {
                    established.fields.insert(*field_id, field.clone());
                }
            }
        }
        next.validate()?;
        *self = next;
        Ok(())
    }

    /// Validate a complete stored successor, where omission would be removal.
    // fuzz: store_contract
    pub fn validate_successor(&self, next: &Schema) -> Result<()> {
        next.validate()?;
        let mut merged = self.clone();
        merged.establish(next)?;
        if &merged != next {
            return Err(Error::InvalidInput("established schema identities cannot be removed"));
        }
        Ok(())
    }
}

fn require_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > 255 || name.contains('\0') {
        return Err(Error::InvalidInput("schema names must contain 1 to 255 bytes"));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum Value {
    Text(String),
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Bytes(Vec<u8>),
    /// Stable reference to an independent named blob. Removing this reference
    /// does not retire the entry or release its retained content.
    File {
        volume_id: Id,
        entry_id: crate::store::EntryId,
    },
    /// References identify data; they confer no permission to read the target.
    Reference {
        volume_id: Id,
        database_id: crate::store::EntryId,
        object_id: Id,
    },
}

/// Originating kvdb HLC, then stable device identity, form the total merge order.
/// This is not a permission check or proof of authorship.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct EditStamp {
    pub clock: u64,
    pub device_id: Id,
}

impl EditStamp {
    pub fn new(clock: Clock, device_id: Id) -> Result<Self> {
        if clock.is_zero() {
            return Err(Error::InvalidInput("object edit clock cannot be zero"));
        }
        Ok(Self { clock: clock.as_u64(), device_id })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FieldEdit {
    pub object_id: Id,
    pub type_id: Id,
    pub field_id: Id,
    pub stamp: EditStamp,
    /// None is a retained tombstone; Some([]) is an empty repeated field.
    pub values: Option<Vec<Value>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct FieldCell {
    pub stamp: EditStamp,
    pub values: Option<Vec<Value>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct ObjectFields {
    pub object_id: Id,
    pub type_id: Id,
    pub fields: BTreeMap<Id, FieldCell>,
}

impl ObjectFields {
    pub fn new(object_id: Id, type_id: Id) -> Self {
        Self { object_id, type_id, fields: BTreeMap::new() }
    }

    /// Merge an already authorized edit. Unmentioned fields remain byte-for-byte
    /// unchanged when encoded individually in kvdb. No whole-object replacement
    /// operation is exposed to guest applications.
    pub fn apply(&mut self, schema: &Schema, edit: &FieldEdit) -> Result<bool> {
        if self.object_id != edit.object_id || self.type_id != edit.type_id {
            return Err(Error::InvalidInput("object edit identity does not match"));
        }
        if edit.stamp.clock == 0 {
            return Err(Error::InvalidInput("object edit clock cannot be zero"));
        }
        let field = schema
            .types
            .get(&self.type_id)
            .and_then(|declaration| declaration.fields.get(&edit.field_id))
            .ok_or(Error::NotFound("object field declaration"))?;
        if let Some(values) = &edit.values {
            if !field.repeated && values.len() != 1 {
                return Err(Error::InvalidInput("scalar object fields require one value"));
            }
            for value in values {
                let valid = match (field.value_type, value) {
                    (ValueType::Text, Value::Text(_))
                    | (ValueType::Boolean, Value::Boolean(_))
                    | (ValueType::Integer, Value::Integer(_))
                    | (ValueType::Bytes, Value::Bytes(_)) => true,
                    (ValueType::File, Value::File { volume_id, .. }) => {
                        *volume_id.as_bytes() != [0; 16]
                    }
                    (ValueType::Reference(_), Value::Reference { volume_id, object_id, .. }) => {
                        *volume_id.as_bytes() != [0; 16] && *object_id.as_bytes() != [0; 16]
                    }
                    (ValueType::Float, Value::Float(value)) => value.is_finite(),
                    _ => false,
                };
                if !valid {
                    return Err(Error::InvalidInput("object field value has the wrong type"));
                }
            }
        }
        if let Some(previous) = self.fields.get(&edit.field_id) {
            if previous.stamp == edit.stamp && previous.values != edit.values {
                return Err(Error::InvalidInput("object edit identity has conflicting values"));
            }
            if previous.stamp >= edit.stamp {
                return Ok(false);
            }
        }
        let previous = self
            .fields
            .insert(edit.field_id, FieldCell { stamp: edit.stamp, values: edit.values.clone() });
        assert!(previous.is_none_or(|cell| cell.stamp < edit.stamp), "merge order advances");
        Ok(true)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    fn id(byte: u8) -> Id {
        Id::from_bytes([byte; 16])
    }

    fn schema() -> Schema {
        Schema {
            types: BTreeMap::from([(
                id(1),
                TypeDeclaration {
                    name: "Note".into(),
                    deprecated: false,
                    fields: BTreeMap::from([(
                        id(2),
                        FieldDeclaration {
                            name: "title".into(),
                            value_type: ValueType::Text,
                            repeated: false,
                            deprecated: false,
                        },
                    )]),
                },
            )]),
        }
    }

    fn edit(field: u8, device: u8, count: u32, text: Option<&str>) -> FieldEdit {
        FieldEdit {
            object_id: id(4),
            type_id: id(1),
            field_id: id(field),
            stamp: EditStamp::new(Clock { time: 1, count }, id(device)).unwrap(),
            values: text.map(|text| vec![Value::Text(text.into())]),
        }
    }

    #[test]
    fn replacement_permissions_do_not_inherit_public_or_owner() {
        let anne = Principal::Human(id(1));
        let bob = Principal::Human(id(2));
        let volume = Acl {
            grants: vec![
                Grant { subject: Subject::Public, permissions: vec![Permission::Read] },
                Grant {
                    subject: Subject::Principal(anne),
                    permissions: vec![Permission::Read, Permission::Write],
                },
            ],
        };
        let object = EntryAcl::Replace(Acl {
            grants: vec![Grant {
                subject: Subject::Principal(bob),
                permissions: vec![Permission::Read],
            }],
        });
        assert!(EntryAcl::Inherit.effective(&volume).permits(None, Permission::Read));
        let acl = object.effective(&volume);
        assert!(acl.permits(Some(bob), Permission::Read));
        assert!(!acl.permits(None, Permission::Read));
        assert!(!acl.permits(Some(anne), Permission::Read));
        assert!(!acl.permits(Some(bob), Permission::Write));
    }

    #[test]
    fn malformed_acls_fail_closed_and_public_writes_require_identity() {
        let public_write = Grant { subject: Subject::Public, permissions: vec![Permission::Write] };
        let acl = Acl { grants: vec![public_write.clone()] };
        assert!(acl.validate().is_ok());
        assert!(acl.permits(Some(Principal::Human(id(1))), Permission::Write));
        assert!(!acl.permits(None, Permission::Write));
        for grants in [
            vec![public_write.clone(), public_write],
            vec![Grant { subject: Subject::Public, permissions: vec![] }],
            vec![Grant {
                subject: Subject::Public,
                permissions: vec![Permission::Read, Permission::Read],
            }],
            vec![Grant {
                subject: Subject::Public,
                permissions: vec![Permission::Read, Permission::ManagePermissions],
            }],
        ] {
            let invalid = Acl { grants };
            assert!(invalid.validate().is_err());
            assert!(!invalid.permits(None, Permission::Read));
        }
        assert!(Acl::default().validate().is_ok());
        assert!(!Acl::default().permits(Some(Principal::Human(id(1))), Permission::Read));
    }

    #[test]
    fn schema_establishment_is_additive_atomic_and_deprecation_is_monotonic() {
        let old = schema();
        let mut current = old.clone();
        let mut added = old.clone();
        let declaration = added.types.get_mut(&id(1)).unwrap();
        declaration.fields.insert(
            id(3),
            FieldDeclaration {
                name: "body".into(),
                value_type: ValueType::Text,
                repeated: false,
                deprecated: false,
            },
        );
        declaration.fields.get_mut(&id(2)).unwrap().deprecated = true;
        current.establish(&added).unwrap();
        current.establish(&old).unwrap();
        assert_eq!(current, added);
        assert!(old.validate_successor(&current).is_ok());
        assert!(current.validate_successor(&old).is_err());
        for mutation in [0, 1, 2, 3] {
            let mut invalid = old.clone();
            let declaration = invalid.types.get_mut(&id(1)).unwrap();
            let field = declaration.fields.get_mut(&id(2)).unwrap();
            match mutation {
                0 => field.name = "renamed".into(),
                1 => field.value_type = ValueType::Integer,
                2 => field.repeated = true,
                _ => declaration.name = "Renamed".into(),
            }
            let before = current.clone();
            assert!(current.establish(&invalid).is_err());
            assert_eq!(current, before);
        }
    }

    #[test]
    fn concurrent_schema_additions_commute_and_name_conflicts_preserve_ids() {
        let base = schema();
        let mut first = base.clone();
        first.types.get_mut(&id(1)).unwrap().fields.insert(
            id(3),
            FieldDeclaration {
                name: "body".into(),
                value_type: ValueType::Text,
                repeated: false,
                deprecated: false,
            },
        );
        let mut second = base.clone();
        second.types.insert(
            id(5),
            TypeDeclaration { name: "Folder".into(), deprecated: false, fields: BTreeMap::new() },
        );
        let mut left = first.clone();
        left.establish(&second).unwrap();
        let mut right = second;
        right.establish(&first).unwrap();
        assert_eq!(left, right);
        let mut collision = Schema::default();
        collision.types.insert(
            id(6),
            TypeDeclaration {
                name: base.types[&id(1)].name.clone(),
                deprecated: false,
                fields: BTreeMap::new(),
            },
        );
        left.establish(&collision).unwrap();
        assert!(matches!(left.type_by_name("Note"), Err(Error::AlreadyExists(_))));
        assert!(left.types.contains_key(&id(1)) && left.types.contains_key(&id(6)));
    }

    #[test]
    fn wire_edits_preserve_unmentioned_fields_and_converge_in_any_order() {
        let mut schema = schema();
        let declaration = schema.types.get_mut(&id(1)).unwrap();
        declaration.fields.insert(
            id(3),
            FieldDeclaration {
                name: "body".into(),
                value_type: ValueType::Text,
                repeated: false,
                deprecated: false,
            },
        );
        let edits = [
            edit(2, 7, 1, Some("First")),
            edit(3, 8, 1, Some("Body")),
            edit(2, 8, 1, Some("Second")),
            edit(2, 7, 2, None),
        ];
        let mut left = ObjectFields::new(id(4), id(1));
        let mut right = left.clone();
        for edit in &edits {
            let wire = serde_json::to_vec(edit).unwrap();
            let decoded: FieldEdit = serde_json::from_slice(&wire).unwrap();
            left.apply(&schema, &decoded).unwrap();
        }
        for edit in edits.iter().rev() {
            right.apply(&schema, edit).unwrap();
        }
        assert_eq!(left, right);
        assert_eq!(left.fields[&id(2)].values, None);
        assert_eq!(left.fields[&id(3)].values, Some(vec![Value::Text("Body".into())]));
        assert!(!left.apply(&schema, &edits[0]).unwrap());
        let mut forged = edits[3].clone();
        forged.values = Some(vec![Value::Text("conflict".into())]);
        assert!(left.apply(&schema, &forged).is_err());
    }

    #[test]
    fn deprecated_fields_remain_writable_and_invalid_edits_do_not_mutate() {
        let mut schema = schema();
        schema.types.get_mut(&id(1)).unwrap().fields.get_mut(&id(2)).unwrap().deprecated = true;
        let mut object = ObjectFields::new(id(4), id(1));
        let valid = edit(2, 7, 1, Some("Readable"));
        object.apply(&schema, &valid).unwrap();
        for mutation in [0, 1, 2, 3, 4] {
            let mut invalid = edit(2, 7, 2, Some("Changed"));
            match mutation {
                0 => invalid.object_id = id(9),
                1 => invalid.field_id = id(9),
                2 => invalid.values = Some(vec![Value::Integer(42)]),
                3 => invalid.values = Some(Vec::new()),
                _ => invalid.stamp.clock = 0,
            }
            let before = object.clone();
            assert!(object.apply(&schema, &invalid).is_err());
            assert_eq!(object, before);
        }
    }

    #[test]
    fn known_field_edit_preserves_unknown_field_and_device_breaks_clock_tie() {
        let schema = schema();
        let mut object = ObjectFields::new(id(4), id(1));
        let unknown = FieldCell {
            stamp: EditStamp::new(Clock { time: 1, count: 1 }, id(8)).unwrap(),
            values: Some(vec![Value::Bytes(vec![1, 2, 3])]),
        };
        object.fields.insert(id(9), unknown.clone());
        let mut other = object.clone();
        let first = edit(2, 7, 1, Some("First"));
        let second = edit(2, 8, 1, Some("Second"));
        object.apply(&schema, &first).unwrap();
        object.apply(&schema, &second).unwrap();
        other.apply(&schema, &second).unwrap();
        other.apply(&schema, &first).unwrap();
        assert_eq!(object, other);
        assert_eq!(object.fields[&id(2)].values, second.values);
        assert_eq!(object.fields[&id(9)], unknown);
    }

    #[test]
    fn field_edit_wire_fixture_is_stable() {
        let fixture = concat!(
            "{\"object_id\":\"Vm7ypzTh7eEsaRsGET44j\",",
            "\"type_id\":\"8C2kCzsB2fJy9MiZos1mS\",",
            "\"field_id\":\"FP4VQzjM4KcwHiS8cj2Xs\",",
            "\"stamp\":{\"clock\":4294967297,",
            "\"device_id\":\"sLDDSz4DCdAn1W1xf36N2\"},",
            "\"values\":[{\"kind\":\"text\",\"value\":\"First\"}]}"
        );
        let expected = edit(2, 7, 1, Some("First"));
        assert_eq!(serde_json::from_str::<FieldEdit>(fixture).unwrap(), expected);
        assert_eq!(serde_json::to_string(&expected).unwrap(), fixture);
    }
}
