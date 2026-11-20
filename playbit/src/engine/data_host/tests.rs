#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;
use crate::runtime::store_bridge::sid;
use crate::store::Access;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[test]
fn creation_fields_fail_without_empty_objects_or_invalidation() {
    let directory = tempfile::tempdir().unwrap();
    let engine = crate::engine::Engine::open(directory.path()).unwrap();
    let volume = engine.services.store.create_volume("Creation fixture", true).unwrap();
    let access = Access::owner(engine.services.store.account().id);
    let changed = Arc::new(AtomicBool::new(false));
    let mut host = DataHost {
        store: engine.services.store.clone(),
        access: access.clone(),
        changed: changed.clone(),
    };
    let schema = Schema {
        types: vec![TypeDeclaration {
            id: [2; 16],
            name: "Task".into(),
            deprecated: false,
            fields: vec![FieldDeclaration {
                id: [3; 16],
                name: "Title".into(),
                value_type: ValueType::Text,
                repeated: false,
                deprecated: false,
            }],
        }],
    };
    let entry = engine
        .services
        .store
        .create_database(
            &access,
            volume.id,
            "data",
            crate::store::volume_index::EntryKind::ObjectDb {
                schema: crate::runtime::store_bridge::to_schema(&schema),
            },
        )
        .unwrap();
    let database = crate::store::volume_index::EntryAddress { volume: volume.id, entry: entry.id };
    let address = crate::runtime::host::EntryAddress {
        volume_id: *volume.id.as_bytes(),
        entry_id: *entry.id.as_bytes(),
    };
    changed.store(false, Ordering::Release);
    let volume_id = address;
    let valid = FieldChange {
        field_id: [3; 16],
        values: Some(vec![Value::Text("A complete task".into())]),
    };
    let invalid = FieldChange { field_id: [3; 16], values: Some(vec![Value::Boolean(true)]) };
    assert_eq!(host.create_object(volume_id, [2; 16], &[invalid]), Err(HostError::InvalidArgument));
    assert_eq!(
        host.create_object(volume_id, [2; 16], &[valid.clone(), valid.clone()]),
        Err(HostError::InvalidArgument)
    );
    assert_eq!(
        host.create_object(volume_id, [8; 16], std::slice::from_ref(&valid)),
        Err(HostError::NotFound)
    );
    host.access = Access::owner(sid([9; 16]));
    assert_eq!(
        host.create_object(volume_id, [2; 16], std::slice::from_ref(&valid)),
        Err(HostError::AccessDenied)
    );
    host.access = access.clone();
    assert!(!changed.load(Ordering::Acquire));
    assert!(
        engine
            .services
            .store
            .query_objects(&access, database, sid([2; 16]), None, 64)
            .unwrap()
            .objects
            .is_empty()
    );
    let id = host.create_object(volume_id, [2; 16], std::slice::from_ref(&valid)).unwrap();
    assert!(changed.load(Ordering::Acquire));
    assert_eq!(host.read_object(volume_id, id, &[[3; 16]]).unwrap().fields, [valid]);
    assert_eq!(
        engine
            .services
            .store
            .query_objects(&access, database, sid([2; 16]), None, 64)
            .unwrap()
            .objects
            .len(),
        1
    );
}
