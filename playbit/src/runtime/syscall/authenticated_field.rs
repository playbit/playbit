use super::*;

pub(in crate::runtime) fn validate_record(
    address: EntryAddress,
    object: Id,
    field: Id,
    record: Option<&AuthenticatedFieldRead>,
) -> HostResult<()> {
    let Some(record) = record else { return Ok(()) };
    if record.volume_id != address.volume_id
        || record.entry_id != address.entry_id
        || record.entry_id == [0; 15]
        || record.object_id != object
        || record.field_id != field
        || record.type_id == [0; 16]
    {
        return Err(HostError::Internal);
    }
    match record.provenance {
        FieldProvenance::Unproven => {}
        FieldProvenance::ApplicationEdit { principal, device_id }
        | FieldProvenance::Rekey { principal, device_id } => {
            let (Principal::Human(id) | Principal::Service(id)) = principal;
            if id == [0; 16] || device_id == [0; 16] {
                return Err(HostError::Internal);
            }
        }
    }
    data_validation::values(record.values.as_deref()).map_err(data_validation::host_error)?;
    Ok(())
}

#[cfg(test)]
#[path = "authenticated_field_tests.rs"]
mod tests;
