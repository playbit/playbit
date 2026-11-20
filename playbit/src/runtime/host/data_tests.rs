#![allow(clippy::unwrap_used, reason = "Tests assert successful bounded fixture operations.")]
use super::*;
use crate::runtime::host::{FieldDeclaration, Host, TypeDeclaration, Value, ValueType};
use crate::store::{Access, LocalStore};
use std::sync::{Arc, atomic::AtomicBool};

#[test]
fn kv_previews_fit_the_reserved_completion_and_reject_invalid_prefixes() {
    // Saturate all key and preview lengths so completion accounting covers the complete retained page.
    use crate::store::key_value::{Key, MAX_KEYS, MAX_STORED_BYTES, PREVIEW_BYTES, Page, Root};
    let call = DataCall {
        volume: [1; 16],
        operation: DataOperation::KvKeys {
            entry_id: [2; 15],
            after: None,
            root: None,
            limit: MAX_KEYS as u32,
        },
    };
    let keys: Vec<_> = (0..MAX_KEYS)
        .map(|index| {
            let mut bytes = vec![0x80; kvdb::KEY_SIZE_MAX];
            *bytes.last_mut().unwrap() = index as u8;
            Key { bytes, len_bytes: MAX_STORED_BYTES as u64, preview: vec![0xa5; PREVIEW_BYTES] }
        })
        .collect();
    let page = Page {
        next: Some(keys.last().unwrap().bytes.clone()),
        keys,
        root: Some(Root { level: 1, digest: [3; 32] }),
    };
    let result = DataResult::KvKeys(page.clone());
    call.validate_result(&result, Default::default()).unwrap();
    assert!(result.retained_bytes().unwrap() <= call.completion_bytes());
    for (preview, total) in [(vec![0; PREVIEW_BYTES + 1], MAX_STORED_BYTES as u64), (vec![0; 2], 1)]
    {
        let mut invalid = page.clone();
        invalid.keys[0].preview = preview;
        invalid.keys[0].len_bytes = total;
        assert_eq!(
            call.validate_result(&DataResult::KvKeys(invalid), Default::default()),
            Err(HostError::Internal)
        );
    }
}

#[test]
fn kv_preview_json_is_additive_and_guest_frame_bytes_are_unchanged() {
    // Freeze the guest frame: internal value previews must never become ABI bytes.
    use crate::store::key_value::{Key, MAX_STORED_BYTES, Page, Root};
    let page = Page {
        keys: vec![Key {
            bytes: vec![0, 0xff],
            len_bytes: MAX_STORED_BYTES as u64,
            preview: vec![0x42; 128],
        }],
        next: None,
        root: Some(Root { level: 7, digest: [0x91; 32] }),
    };
    let result = DataResult::KvKeys(page.clone());
    let mut frame = vec![0xcc; result.kv_output_bytes().unwrap()];
    assert_eq!(result.encode_kv(&mut frame).unwrap(), frame.len());
    let mut expected = vec![1, 7];
    expected.extend_from_slice(&[0x91; 32]);
    expected.extend_from_slice(&[1, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0x40, 0, 0, 0, 0, 0, 0, 0xff]);
    assert_eq!(frame, expected);
    let mut without = page.clone();
    without.keys[0].preview.clear();
    let without = DataResult::KvKeys(without);
    let mut other_frame = vec![0; without.kv_output_bytes().unwrap()];
    without.encode_kv(&mut other_frame).unwrap();
    assert_eq!(frame, other_frame);
    #[cfg(feature = "app-host")]
    {
        let mut legacy = serde_json::to_value(&page).unwrap();
        assert_eq!(legacy["keys"][0]["preview"].as_array().unwrap().len(), 128);
        legacy["keys"][0].as_object_mut().unwrap().remove("preview");
        let decoded: Page = serde_json::from_value(legacy).unwrap();
        assert!(decoded.keys[0].preview.is_empty());
    }
}

#[test]
fn only_the_exact_typed_mutation_acknowledgment_is_a_receipt() {
    // Exercise the result-to-kind mapping which the scalar predicate proof
    // abstracts. Written is intentionally shared by both write and commit.
    for (result, kind) in [
        (DataResult::Created([1; 16]), DataKind::Create),
        (DataResult::Edited, DataKind::Edit),
        (DataResult::Written, DataKind::FileWrite),
        (DataResult::SchemaEstablished, DataKind::SchemaEstablish),
        (DataResult::FileTransfer { transfer: [2; 16] }, DataKind::FileTransferStart),
        (DataResult::FileTransferred { offset: 3 }, DataKind::FileTransferChunk),
    ] {
        for raw in 1..=16 {
            let expected = DataKind::from_raw(raw).unwrap();
            assert_eq!(
                result.is_receipt(expected),
                expected == kind
                    || (kind == DataKind::FileWrite && expected == DataKind::FileTransferCommit)
            );
        }
    }
    assert!(!DataResult::Schema(Default::default()).is_receipt(DataKind::SchemaEstablish));
    assert!(
        !DataResult::File(FileChunk { total_bytes: 1, bytes: vec![7] })
            .is_receipt(DataKind::FileWrite)
    );
    assert!(
        DataResult::BloblogAppended(
            crate::store::bloblog::Cursor::parse(&format!(
                "1/{}/{}/{}/0/{}",
                crate::store::Id::from_bytes([0; 16]),
                crate::store::EntryId::from_bytes([1; 15]).unwrap(),
                crate::identifier::encode(&[0; 8]),
                crate::store::Id::from_bytes([1; 16]),
            ))
            .unwrap()
        )
        .is_receipt(DataKind::BloblogAppend)
    );
    #[cfg(feature = "connected-engine")]
    assert!(!DataResult::ChannelCompleted.is_receipt(DataKind::ChannelCommand));
}

#[cfg(feature = "connected-engine")]
#[test]
fn channel_contract_bounds_payloads_batches_and_completion_storage() {
    use crate::{
        engine::session_channels::{EVENT_LIMIT, Event},
        session_channel::MAX_MESSAGE_BYTES as SESSION_MESSAGE_LIMIT,
        store::Id,
    };
    let id = Id::from_bytes([81; 16]).to_string();
    let send = DataCall {
        volume: [82; 16],
        operation: DataOperation::ChannelCommand {
            instance: [1; 16],
            name: "test".into(),
            action: 1,
            data: vec![7; SESSION_MESSAGE_LIMIT],
        },
    };
    assert!(!send.operation.tool_only());
    assert!(send.operation.network());
    assert!(!send.operation.is_mutation());
    assert_eq!(send.operation.rights(), VolumeRights::READ);
    send.operation.validate(Default::default()).unwrap();
    assert_eq!(
        DataOperation::ChannelCommand {
            instance: [1; 16],
            name: "test".into(),
            action: 1,
            data: vec![0; SESSION_MESSAGE_LIMIT + 1]
        }
        .validate(Default::default()),
        Err(HostError::LimitExceeded)
    );
    let sent = DataResult::ChannelCompleted;
    send.validate_result(&sent, Default::default()).unwrap();
    assert!(sent.retained_bytes().unwrap() <= send.completion_bytes());
    let read = DataCall {
        volume: [82; 16],
        operation: DataOperation::ChannelEvents { instance: [1; 16] },
    };
    let page = vec![
        Event::Message {
            channel: "test".into(),
            sender: crate::session_channel::Participant {
                account_id: id.clone(),
                device_id: id.clone(),
                instance_id: id.clone(),
                build_id: crate::identifier::encode(&[1; 32])
            },
            message_id: id,
            data: vec![7; 512]
        };
        EVENT_LIMIT
    ];
    let result = DataResult::ChannelEvents(page.clone());
    read.validate_result(&result, Default::default()).unwrap();
    assert!(result.retained_bytes().unwrap() <= read.completion_bytes());
    let bytes =
        crate::engine::ipc::codec::encode(&result, crate::engine::ipc::codec::MAX_ARCHIVE).unwrap();
    let decoded = rkyv::api::low::from_bytes::<DataResult, rkyv::rancor::Error>(&bytes).unwrap();
    assert_eq!(decoded, result);
    let mut too_many = page;
    too_many.push(Event::Presence { participants: vec![] });
    assert!(
        read.validate_result(&DataResult::ChannelEvents(too_many), Default::default()).is_err()
    );
}

#[test]
fn bloblog_contract_bounds_identity_scope_and_completion_storage() {
    use crate::store::bloblog;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    let volume = store.create_volume("Tool log", true).unwrap().id;
    let access = Access::owner(store.account().id);
    let log = store.create_bloblog(&access, volume, "activity").unwrap().id;
    let blobs = vec![vec![], vec![7; bloblog::MAX_BLOB_BYTES]];
    let append = DataCall {
        volume: *volume.as_bytes(),
        operation: DataOperation::BloblogAppend {
            append_id: [1; 16],
            log: *log.as_bytes(),
            blobs: blobs.clone(),
        },
    };
    assert!(!append.operation.tool_only());
    assert!(append.operation.is_mutation());
    assert_eq!(append.operation.rights(), VolumeRights::WRITE);
    append.operation.validate(Default::default()).unwrap();
    let cursor = store.bloblog_put(&access, volume, log, &blobs).unwrap();
    let acknowledged = DataResult::BloblogAppended(cursor.clone());
    append.validate_result(&acknowledged, Default::default()).unwrap();
    assert!(acknowledged.retained_bytes().unwrap() <= append.completion_bytes());
    let read = DataCall {
        volume: *volume.as_bytes(),
        operation: DataOperation::BloblogRead { log: *log.as_bytes(), cursor: None },
    };
    assert!(!read.operation.tool_only());
    assert!(!read.operation.is_mutation());
    let page = store.bloblog_get(&access, volume, log, None).unwrap();
    let valid = DataResult::BloblogPage(page.clone());
    read.validate_result(&valid, Default::default()).unwrap();
    assert!(valid.retained_bytes().unwrap() <= read.completion_bytes());
    let mut wrong = read.clone();
    wrong.volume = [92; 16];
    assert_eq!(wrong.validate_result(&valid, Default::default()), Err(HostError::Internal));
    let mut wrong_volume = append.clone();
    wrong_volume.volume = [92; 16];
    assert_eq!(
        wrong_volume.validate_result(&acknowledged, Default::default()),
        Err(HostError::Internal)
    );
    let mut wrong_log = append.clone();
    wrong_log.operation =
        DataOperation::BloblogAppend { append_id: [1; 16], log: [93; 15], blobs: vec![vec![]] };
    assert_eq!(
        wrong_log.validate_result(&acknowledged, Default::default()),
        Err(HostError::Internal)
    );
    let mut corrupt = page;
    corrupt.entries[1].data[0] ^= 1;
    assert!(read.validate_result(&DataResult::BloblogPage(corrupt), Default::default()).is_err());
    for invalid in [
        vec![],
        vec![vec![]; bloblog::MAX_BLOBS + 1],
        vec![vec![0; bloblog::MAX_BLOB_BYTES + 1]],
        vec![vec![0; bloblog::MAX_BLOB_BYTES]; 2],
    ] {
        assert!(
            DataOperation::BloblogAppend {
                append_id: [1; 16],
                log: *log.as_bytes(),
                blobs: invalid
            }
            .validate(Default::default())
            .is_err()
        );
    }
    let mut host = crate::engine::data_host::DataHost {
        store,
        access,
        changed: Arc::new(AtomicBool::new(false)),
    };
    assert_eq!(append.execute(&mut host), Err(HostError::Unavailable));
    assert!(!host.changed.load(std::sync::atomic::Ordering::Acquire));
    let DataResult::BloblogPage(page) = read.execute(&mut host).unwrap() else { panic!() };
    assert_eq!(page.entries.len(), 2);
    assert!(!page.more);
}

#[test]
fn projected_reads_and_edits_preserve_unrequested_large_cells() {
    use crate::store::object_contract as stored;
    use std::collections::BTreeMap;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    let volume = store.create_volume("Bounded cells", true).unwrap();
    let mut host = crate::engine::data_host::DataHost {
        access: Access::owner(store.account().id),
        store: store.clone(),
        changed: Arc::new(AtomicBool::new(false)),
    };
    let entry = store
        .create_database(
            &host.access,
            volume.id,
            "data",
            crate::store::volume_index::EntryKind::ObjectDb { schema: Default::default() },
        )
        .unwrap();
    let database = crate::store::volume_index::EntryAddress { volume: volume.id, entry: entry.id };
    let address = crate::runtime::host::EntryAddress {
        volume_id: *volume.id.as_bytes(),
        entry_id: *entry.id.as_bytes(),
    };
    host.establish_schema(
        address,
        &Schema {
            types: vec![TypeDeclaration {
                id: [1; 16],
                name: "Record".into(),
                deprecated: false,
                fields: vec![
                    FieldDeclaration {
                        id: [2; 16],
                        name: "Title".into(),
                        value_type: ValueType::Text,
                        repeated: false,
                        deprecated: false,
                    },
                    FieldDeclaration {
                        id: [3; 16],
                        name: "Large cell".into(),
                        value_type: ValueType::Bytes,
                        repeated: true,
                        deprecated: false,
                    },
                ],
            }],
        },
    )
    .unwrap();
    let field = |byte| crate::store::Id::from_bytes([byte; 16]);
    // A large valid object can be read through a small field projection. Edits
    // validate the whole object but preserve unrelated stored cells. Leave one
    // value for the title within the whole-object 4096-value bound.
    let large = vec![stored::Value::Bytes(vec![7; 128]); 4095];
    let object = store
        .create_object_with_fields(
            &host.access,
            database,
            field(1),
            &BTreeMap::from([(field(2), Some(vec![stored::Value::Text("Before".into())]))]),
        )
        .unwrap();
    let record = store
        .read_object_fields(&host.access, database, object.object_id, Some(&[field(3)]))
        .unwrap();
    let schema = store.schema(&host.access, database).unwrap();
    store
        .with_replica(volume.id, crate::store::DatabaseId::Entry(entry.id), |db| {
            let mut tx = db.begin_write(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            )?;
            crate::store::object_database::edit(
                &mut tx,
                &schema,
                record,
                store.account().id,
                &BTreeMap::from([(field(3), Some(large.clone()))]),
            )?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
    let object_id = *object.object_id.as_bytes();
    let small = host.read_object(address, object_id, &[[2; 16]]).unwrap();
    assert_eq!(small.fields[0].values, Some(vec![Value::Text("Before".into())]));
    let complete = host.read_object(address, object_id, &[[2; 16], [3; 16]]).unwrap();
    assert_eq!(complete.fields.len(), 2);
    assert_eq!(complete.fields[1].values.as_ref().unwrap().len(), 4095);
    host.edit_fields(
        address,
        object_id,
        &[FieldChange { field_id: [2; 16], values: Some(vec![Value::Text("After".into())]) }],
    )
    .unwrap();
    let record = store
        .read_object_fields(&host.access, database, object.object_id, Some(&[field(3)]))
        .unwrap();
    assert_eq!(
        host.read_object(address, object_id, &[[2; 16]]).unwrap().fields[0].values,
        Some(vec![Value::Text("After".into())])
    );
    assert_eq!(record.fields[&field(3)].values, Some(large));
}

#[test]
fn shared_data_dispatch_commits_complete_rows_and_preserves_authority() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    let volume = store.create_volume("Async data fixture", true).unwrap();
    let mut host = crate::engine::data_host::DataHost {
        access: Access::owner(store.account().id),
        store: store.clone(),
        changed: Arc::new(AtomicBool::new(false)),
    };
    let entry = store
        .create_database(
            &host.access,
            volume.id,
            "data",
            crate::store::volume_index::EntryKind::ObjectDb { schema: Default::default() },
        )
        .unwrap();
    let execute = |host: &mut dyn Host, operation| {
        DataCall { volume: *volume.id.as_bytes(), operation }.execute(host)
    };
    execute(
        &mut host,
        DataOperation::SchemaEstablish {
            entry_id: *entry.id.as_bytes(),
            schema: Schema {
                types: vec![TypeDeclaration {
                    id: [1; 16],
                    name: "Task".into(),
                    deprecated: false,
                    fields: vec![FieldDeclaration {
                        id: [2; 16],
                        name: "done".into(),
                        value_type: ValueType::Boolean,
                        repeated: false,
                        deprecated: false,
                    }],
                }],
            },
        },
    )
    .unwrap();
    let fields = vec![FieldChange { field_id: [2; 16], values: Some(vec![Value::Boolean(false)]) }];
    let DataResult::Created(object_id) = execute(
        &mut host,
        DataOperation::Create {
            entry_id: *entry.id.as_bytes(),
            type_id: [1; 16],
            fields: fields.clone(),
        },
    )
    .unwrap() else {
        panic!("creation result");
    };
    let DataResult::Object(object) = execute(
        &mut host,
        DataOperation::Read { entry_id: *entry.id.as_bytes(), object_id, fields: vec![[2; 16]] },
    )
    .unwrap() else {
        panic!("read result");
    };
    assert_eq!(object.fields, fields);
    assert_eq!(
        execute(
            &mut host,
            DataOperation::Create {
                entry_id: *entry.id.as_bytes(),
                type_id: [1; 16],
                fields: vec![FieldChange {
                    field_id: [2; 16],
                    values: Some(vec![Value::Text("invalid".into())])
                },]
            }
        ),
        Err(HostError::InvalidArgument)
    );
    let DataResult::Page(page) = execute(
        &mut host,
        DataOperation::Query {
            entry_id: *entry.id.as_bytes(),
            type_id: [1; 16],
            after: None,
            limit: 8,
            projection: Some(vec![[2; 16]]),
        },
    )
    .unwrap() else {
        panic!("query result");
    };
    assert_eq!(page.objects, vec![object_id]);
    assert_eq!(page.records.unwrap()[0].fields, fields);
    assert_eq!(
        execute(
            &mut host,
            DataOperation::Edit {
                entry_id: *entry.id.as_bytes(),
                object_id,
                fields: vec![FieldChange {
                    field_id: [2; 16],
                    values: Some(vec![Value::Boolean(true)])
                },]
            }
        ),
        Ok(DataResult::Edited)
    );
    host.access = Access::owner(crate::store::Id::from_bytes([99; 16]));
    assert_eq!(
        execute(
            &mut host,
            DataOperation::Read {
                entry_id: *entry.id.as_bytes(),
                object_id,
                fields: vec![[2; 16]]
            }
        ),
        Err(HostError::AccessDenied)
    );
}

#[test]
fn malformed_data_inputs_are_rejected_before_dispatch() {
    let limits = QueryLimits { records: 8, value_bytes: 8192 };
    for (limit, projection, error) in [
        (0, None, HostError::InvalidArgument),
        (9, None, HostError::LimitExceeded),
        (1, Some(vec![]), HostError::InvalidArgument),
        (1, Some(vec![[2; 16], [2; 16]]), HostError::InvalidArgument),
    ] {
        assert_eq!(
            DataOperation::Query {
                entry_id: [5; 15],
                type_id: [1; 16],
                after: None,
                limit,
                projection
            }
            .validate(limits),
            Err(error)
        );
    }
    assert_eq!(
        DataOperation::Edit { entry_id: [5; 15], object_id: [1; 16], fields: vec![] }
            .validate(limits),
        Err(HostError::InvalidArgument)
    );
    assert_eq!(
        DataOperation::FileRead { entry_id: [5; 15], offset: u64::MAX, limit: 1 }.validate(limits),
        Err(HostError::InvalidArgument)
    );
}

#[cfg(feature = "app-host")]
#[test]
fn data_api_identifiers_use_canonical_text() {
    use crate::runtime::host::{FieldProvenance, Principal};
    let call = DataCall {
        volume: [1; 16],
        operation: DataOperation::Create {
            entry_id: [5; 15],
            type_id: [2; 16],
            fields: vec![FieldChange { field_id: [3; 16], values: None }],
        },
    };
    let json = serde_json::to_value(&call).unwrap();
    assert_eq!(json["volume"], crate::identifier::encode(&[1; 16]));
    assert_eq!(json["operation"]["Create"]["entry_id"], crate::identifier::encode(&[5; 15]));
    assert_eq!(json["operation"]["Create"]["type_id"], crate::identifier::encode(&[2; 16]));
    assert_eq!(
        json["operation"]["Create"]["fields"][0]["field_id"],
        crate::identifier::encode(&[3; 16])
    );
    assert_eq!(serde_json::from_value::<DataCall>(json).unwrap(), call);
    let result = DataResult::Created([4; 16]);
    assert_eq!(
        serde_json::to_value(&result).unwrap()["Created"],
        crate::identifier::encode(&[4; 16])
    );
    let provenance = FieldProvenance::ApplicationEdit {
        principal: Principal::Human([6; 16]),
        device_id: [7; 16],
    };
    let json = serde_json::to_value(provenance).unwrap();
    assert_eq!(json["ApplicationEdit"]["device_id"], crate::identifier::encode(&[7; 16]));
    assert_eq!(json["ApplicationEdit"]["principal"]["Human"], crate::identifier::encode(&[6; 16]));
}

#[cfg(feature = "app-host")]
#[test]
fn data_calls_and_results_use_the_landed_binary_codec() {
    use crate::engine::ipc::codec;
    let call = DataCall {
        volume: [1; 16],
        operation: DataOperation::Create {
            entry_id: [5; 15],
            type_id: [2; 16],
            fields: vec![FieldChange {
                field_id: [3; 16],
                values: Some(vec![Value::Text("Owned initial value".into())]),
            }],
        },
    };
    let encoded = codec::encode(&call, codec::MAX_ARCHIVE).unwrap();
    let archive = codec::access::<ArchivedDataCall>(&encoded).unwrap();
    let decoded: DataCall = rkyv::api::low::deserialize::<_, rkyv::rancor::Error>(archive).unwrap();
    assert_eq!(decoded, call);
    let result = DataResult::Object(ObjectRecord {
        id: [4; 16],
        type_id: [2; 16],
        fields: vec![FieldChange { field_id: [3; 16], values: Some(vec![Value::Boolean(true)]) }],
    });
    let encoded = codec::encode(&result, codec::MAX_ARCHIVE).unwrap();
    let archive = codec::access::<ArchivedDataResult>(&encoded).unwrap();
    let decoded: DataResult =
        rkyv::api::low::deserialize::<_, rkyv::rancor::Error>(archive).unwrap();
    assert_eq!(decoded, result);
}
