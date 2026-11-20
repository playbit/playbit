//! Common completion validation applies equally to native, Wasm and tool data.
use super::*;
use crate::runtime::host::QueryLimits;

fn record() -> AuthenticatedFieldRead {
    AuthenticatedFieldRead {
        entry_id: [5; 15],
        volume_id: [1; 16],
        object_id: [2; 16],
        type_id: [3; 16],
        field_id: [4; 16],
        revision: FieldRevision([7; 32]),
        values: Some(vec![Value::Integer(23)]),
        provenance: FieldProvenance::Unproven,
    }
}
fn validate(record: Option<AuthenticatedFieldRead>) -> HostResult<()> {
    crate::runtime::host::DataCall {
        volume: [1; 16],
        operation: crate::runtime::host::DataOperation::AuthenticatedField {
            entry_id: [5; 15],
            object_id: [2; 16],
            field_id: [4; 16],
        },
    }
    .validate_result(
        &crate::runtime::host::DataResult::AuthenticatedField(record),
        QueryLimits::default(),
    )
}
#[test]
fn authenticated_completion_keeps_all_value_kinds_and_provenance_distinctions() {
    assert_eq!(validate(None), Ok(()));
    for kind in 0..3 {
        for principal in [Principal::Human([5; 16]), Principal::Service([5; 16])] {
            for values in [
                None,
                Some(vec![]),
                Some(vec![
                    Value::Text("submitted".into()),
                    Value::Boolean(true),
                    Value::Integer(-23),
                    Value::Float(1.5),
                    Value::Bytes(vec![0, 255]),
                    Value::Reference {
                        database_id: [5; 15],
                        volume_id: [1; 16],
                        object_id: [2; 16],
                    },
                    Value::File { volume_id: [1; 16], entry_id: [5; 15] },
                ]),
            ] {
                let mut record = record();
                record.values = values;
                record.provenance = match kind {
                    0 => FieldProvenance::Unproven,
                    1 => FieldProvenance::ApplicationEdit { principal, device_id: [6; 16] },
                    _ => FieldProvenance::Rekey { principal, device_id: [6; 16] },
                };
                assert_eq!(validate(Some(record)), Ok(()));
            }
        }
    }
}
#[test]
fn malformed_authenticated_completions_fail_before_any_output() {
    for case in 0..9 {
        let mut record = record();
        match case {
            0 => record.volume_id = [9; 16],
            1 => record.object_id = [9; 16],
            2 => record.field_id = [9; 16],
            3 => record.type_id = [0; 16],
            4 => {
                record.provenance = FieldProvenance::ApplicationEdit {
                    principal: Principal::Human([0; 16]),
                    device_id: [6; 16],
                }
            }
            5 => {
                record.provenance = FieldProvenance::Rekey {
                    principal: Principal::Service([5; 16]),
                    device_id: [0; 16],
                }
            }
            6 => record.values = Some(vec![Value::Float(f64::NAN)]),
            7 => record.values = Some(vec![Value::Integer(0); 4097]),
            8 => record.values = Some(vec![Value::Bytes(vec![0; 1048577])]),
            _ => unreachable!(),
        }
        assert_eq!(
            validate(Some(record)),
            Err(if case >= 7 { HostError::LimitExceeded } else { HostError::Internal })
        );
    }
}
#[test]
fn authenticated_request_rejects_zero_ids_before_host_admission() {
    for (object_id, field_id) in [([0; 16], [4; 16]), ([2; 16], [0; 16])] {
        assert_eq!(
            crate::runtime::host::DataOperation::AuthenticatedField {
                entry_id: [5; 15],
                object_id,
                field_id
            }
            .validate(QueryLimits::default()),
            Err(HostError::InvalidArgument)
        );
    }
}
