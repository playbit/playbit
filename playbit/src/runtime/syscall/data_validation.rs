//! Semantic data admission shared by typed operations, independent of encoding.
use super::{
    FieldChange, HostError, HostResult, Id, MAX_SCHEMA_FIELDS, MAX_SCHEMA_TYPES, MAX_VALUES,
    ObjectPage, Schema, Value, abi,
};
use std::collections::BTreeSet;

// Logical data admission stays independent of the larger scene/call envelope.
const MAX_DATA_BYTES: usize = abi::MAX_DATA_BYTES;

fn add_bytes(total: &mut usize, amount: usize) -> HostResult<()> {
    assert!(*total <= MAX_DATA_BYTES, "payload accounting stays within admission");
    if amount > MAX_DATA_BYTES - *total {
        return Err(HostError::LimitExceeded);
    }
    *total += amount;
    Ok(())
}

pub(in crate::runtime) fn ids(ids: &[Id]) -> HostResult<()> {
    if ids.len() > abi::MAX_FIELDS as usize {
        return Err(HostError::LimitExceeded);
    }
    if ids.is_empty() {
        return Err(HostError::InvalidArgument);
    }
    let mut seen = BTreeSet::new();
    for id in ids {
        if !seen.insert(id) {
            return Err(HostError::InvalidArgument);
        }
    }
    Ok(())
}

pub(in crate::runtime) fn values(values: Option<&[Value]>) -> HostResult<usize> {
    let Some(values) = values else { return Ok(0) };
    if values.len() > MAX_VALUES as usize {
        return Err(HostError::LimitExceeded);
    }
    let mut bytes = 0;
    for value in values {
        let size = match value {
            Value::Text(value) => value.len(),
            Value::Bytes(value) => value.len(),
            Value::Boolean(_) => 1,
            Value::Integer(_) => 8,
            Value::Float(value) => {
                if !value.is_finite() {
                    return Err(HostError::InvalidArgument);
                }
                8
            }
            Value::Reference { volume_id, database_id, object_id } => {
                if *volume_id == [0; 16] || *database_id == [0; 15] || *object_id == [0; 16] {
                    return Err(HostError::InvalidArgument);
                }
                47
            }
            Value::File { volume_id, entry_id } => {
                if *volume_id == [0; 16] || *entry_id == [0; 15] {
                    return Err(HostError::InvalidArgument);
                }
                31
            }
        };
        add_bytes(&mut bytes, size)?;
    }
    Ok(bytes)
}

pub(in crate::runtime) fn changes(changes: &[FieldChange]) -> HostResult<usize> {
    if changes.len() > abi::MAX_FIELDS as usize {
        return Err(HostError::LimitExceeded);
    }
    let mut seen = BTreeSet::new();
    let mut total_values = 0;
    let mut bytes = 0;
    for field in changes {
        if !seen.insert(field.field_id) {
            return Err(HostError::InvalidArgument);
        }
        let count = field.values.as_ref().map_or(0, Vec::len);
        if count > MAX_VALUES as usize - total_values {
            return Err(HostError::LimitExceeded);
        }
        total_values += count;
        add_bytes(&mut bytes, values(field.values.as_deref())?)?;
    }
    Ok(bytes)
}

pub(in crate::runtime) fn schema(schema: &Schema) -> HostResult<()> {
    if schema.types.len() > MAX_SCHEMA_TYPES as usize {
        return Err(HostError::LimitExceeded);
    }
    let mut identities = BTreeSet::new();
    let mut fields = 0;
    let mut bytes = 0;
    for ty in &schema.types {
        schema_name(&ty.name)?;
        add_bytes(&mut bytes, ty.name.len())?;
        if ty.id == [0; 16] || !identities.insert(ty.id) {
            return Err(HostError::InvalidArgument);
        }
        if ty.fields.len() > abi::MAX_FIELDS as usize
            || ty.fields.len() > MAX_SCHEMA_FIELDS as usize - fields
        {
            return Err(HostError::LimitExceeded);
        }
        fields += ty.fields.len();
        for field in &ty.fields {
            schema_name(&field.name)?;
            add_bytes(&mut bytes, field.name.len())?;
            if field.id == [0; 16] || !identities.insert(field.id) {
                return Err(HostError::InvalidArgument);
            }
        }
    }
    Ok(())
}

fn schema_name(name: &str) -> HostResult<()> {
    if name.trim().is_empty() || name.len() > 255 || name.chars().any(char::is_control) {
        return Err(HostError::InvalidArgument);
    }
    Ok(())
}

/// Malformed host records are a host contract failure, not bad app arguments.
pub(in crate::runtime) fn host_error(error: HostError) -> HostError {
    if error == HostError::InvalidArgument { HostError::Internal } else { error }
}

pub(in crate::runtime) fn query_page(
    page: &ObjectPage,
    type_id: Id,
    after: Option<Id>,
    limit: u32,
    projection: Option<&[Id]>,
    value_bytes: u32,
) -> HostResult<()> {
    if page.objects.len() > limit as usize
        || page.objects.windows(2).any(|ids| ids[0] >= ids[1])
        || page.objects.iter().any(|id| after.is_some_and(|after| *id <= after))
        || page.next.is_some_and(|next| page.objects.last() != Some(&next))
    {
        return Err(HostError::Internal);
    }
    match (projection, &page.records) {
        (None, None) => {}
        (Some(fields), Some(records)) if records.len() == page.objects.len() => {
            let mut bytes = 0;
            let mut value_count = 0usize;
            for (record, id) in records.iter().zip(&page.objects) {
                value_count += record
                    .fields
                    .iter()
                    .filter_map(|field| field.values.as_ref())
                    .map(Vec::len)
                    .sum::<usize>();
                if value_count > abi::MAX_PAGE_VALUES as usize {
                    return Err(HostError::LimitExceeded);
                }
                add_bytes(&mut bytes, changes(&record.fields).map_err(host_error)?)?;
                if bytes > value_bytes as usize {
                    return Err(HostError::LimitExceeded);
                }
                if record.id != *id
                    || record.type_id != type_id
                    || record.fields.iter().any(|field| !fields.contains(&field.field_id))
                {
                    return Err(HostError::Internal);
                }
            }
        }
        _ => return Err(HostError::Internal),
    }
    Ok(())
}
