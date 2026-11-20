//! Typed app data conversions, selected with an app runtime.
use super::super::host::*;
use super::error;
use crate::store::{Id as StoreId, object_contract as data};
use playbit_api::abi::syscall::Id;
use std::collections::BTreeMap;

pub(crate) fn sid(id: Id) -> StoreId {
    StoreId::from_bytes(id)
}
#[cfg(not(target_arch = "wasm32"))]
pub fn from_owned_value(value: data::Value) -> Value {
    match value {
        data::Value::Text(value) => Value::Text(value),
        data::Value::Boolean(value) => Value::Boolean(value),
        data::Value::Integer(value) => Value::Integer(value),
        data::Value::Float(value) => Value::Float(value),
        data::Value::Bytes(value) => Value::Bytes(value),
        data::Value::File { volume_id, entry_id } => {
            Value::File { volume_id: *volume_id.as_bytes(), entry_id: *entry_id.as_bytes() }
        }
        data::Value::Reference { volume_id, database_id, object_id } => Value::Reference {
            volume_id: *volume_id.as_bytes(),
            database_id: *database_id.as_bytes(),
            object_id: *object_id.as_bytes(),
        },
    }
}
/// References identify data and confer no authority, including dangling targets.
pub(crate) fn to_value(value: &Value) -> HostResult<data::Value> {
    Ok(match value {
        Value::Text(v) => data::Value::Text(v.clone()),
        Value::Boolean(v) => data::Value::Boolean(*v),
        Value::Integer(v) => data::Value::Integer(*v),
        Value::Float(v) => data::Value::Float(*v),
        Value::Bytes(v) => data::Value::Bytes(v.clone()),
        Value::File { volume_id, entry_id } => data::Value::File {
            volume_id: sid(*volume_id),
            entry_id: crate::store::EntryId::from_bytes(*entry_id).map_err(error)?,
        },
        Value::Reference { volume_id, database_id, object_id } => data::Value::Reference {
            volume_id: sid(*volume_id),
            object_id: sid(*object_id),
            database_id: crate::store::EntryId::from_bytes(*database_id).map_err(error)?,
        },
    })
}
#[cfg(not(target_arch = "wasm32"))]
fn from_type(value: data::ValueType) -> ValueType {
    match value {
        data::ValueType::Text => ValueType::Text,
        data::ValueType::Boolean => ValueType::Boolean,
        data::ValueType::Integer => ValueType::Integer,
        data::ValueType::Float => ValueType::Float,
        data::ValueType::Bytes => ValueType::Bytes,
        data::ValueType::File => ValueType::File,
        data::ValueType::Reference(id) => ValueType::Reference(*id.as_bytes()),
    }
}
fn to_type(value: ValueType) -> data::ValueType {
    match value {
        ValueType::Text => data::ValueType::Text,
        ValueType::Boolean => data::ValueType::Boolean,
        ValueType::Integer => data::ValueType::Integer,
        ValueType::Float => data::ValueType::Float,
        ValueType::Bytes => data::ValueType::Bytes,
        ValueType::File => data::ValueType::File,
        ValueType::Reference(id) => data::ValueType::Reference(sid(id)),
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub fn from_schema(schema: data::Schema) -> Schema {
    Schema {
        types: schema
            .types
            .into_iter()
            .map(|(id, ty)| TypeDeclaration {
                id: *id.as_bytes(),
                name: ty.name,
                deprecated: ty.deprecated,
                fields: ty
                    .fields
                    .into_iter()
                    .map(|(id, f)| FieldDeclaration {
                        id: *id.as_bytes(),
                        name: f.name,
                        value_type: from_type(f.value_type),
                        repeated: f.repeated,
                        deprecated: f.deprecated,
                    })
                    .collect(),
            })
            .collect(),
    }
}
pub(crate) fn to_schema(schema: &Schema) -> data::Schema {
    data::Schema {
        types: schema
            .types
            .iter()
            .map(|ty| {
                (
                    sid(ty.id),
                    data::TypeDeclaration {
                        name: ty.name.clone(),
                        deprecated: ty.deprecated,
                        fields: ty
                            .fields
                            .iter()
                            .map(|f| {
                                (
                                    sid(f.id),
                                    data::FieldDeclaration {
                                        name: f.name.clone(),
                                        value_type: to_type(f.value_type),
                                        repeated: f.repeated,
                                        deprecated: f.deprecated,
                                    },
                                )
                            })
                            .collect::<BTreeMap<_, _>>(),
                    },
                )
            })
            .collect(),
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn database_address(
    address: crate::runtime::host::EntryAddress,
) -> HostResult<crate::store::volume_index::EntryAddress> {
    Ok(crate::store::volume_index::EntryAddress {
        volume: sid(address.volume_id),
        entry: crate::store::EntryId::from_bytes(address.entry_id).map_err(error)?,
    })
}
