//! Shared operations and guest-boundary conformance tests.
#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use std::sync::{Arc, Mutex};

use crate::runtime::ExecutionBackend;

use super::test_support::{call, read, write};
use super::*;

const ROOT: Id = [1; 16];
const TYPE: Id = [2; 16];
const FIELD: Id = [3; 16];
const OBJECT: Id = [9; 16];
const CALLER: &str = r#"(module
    (import "playbit" "pb_syscall" (func $dispatch_log (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_view (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_event (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_identity (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_volumes (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_create (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_edit (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_defer (param i32 i64 i64 i64 i64 i64 i64) (result i64)))

    ;; Test guests retain a handle returned in the scalar result for later exports.
    (func $save_handle (param $output i32) (param $result i64) (result i32)
        (if (result i32) (i64.lt_s (local.get $result) (i64.const 0))
            (then (i32.wrap_i64 (local.get $result)))
            (else (i64.store (local.get $output) (local.get $result)) (i32.const 0))))
    (func $log (param i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_log (i32.const 2)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $view (param i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_view (i32.const 3)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $event (param i32 i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_event (i32.const 4)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.extend_i32_u (local.get 2)) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $identity (param i32) (result i32)
        (i32.wrap_i64 (call $dispatch_identity (i32.const 5)
            (i64.extend_i32_u (local.get 0)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $volumes (param i32) (result i32)
        (i32.wrap_i64 (call $dispatch_volumes (i32.const 7)
            (i64.extend_i32_u (local.get 0)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $create_begin (param i32 i32 i32 i32 i32) (result i32)
        (call $save_handle (i32.wrap_i64 (i64.extend_i32_u (local.get 4))) (call $dispatch_create (i32.const 276)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1))
            (i64.extend_i32_u (local.get 2)) (i64.extend_i32_u (local.get 3))
            (i64.const 40) (i64.const 0))))
    (func $edit_begin (param i32 i32 i32 i32 i32) (result i32)
        (call $save_handle (i32.wrap_i64 (i64.extend_i32_u (local.get 4))) (call $dispatch_edit (i32.const 306)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1))
            (i64.extend_i32_u (local.get 2)) (i64.extend_i32_u (local.get 3))
            (i64.const 40) (i64.const 0))))

    (func $defer (param i64) (result i32)
        (i32.wrap_i64 (call $dispatch_defer (i32.const 9)
            (local.get 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func (export "invoke") (param i32 i64 i64 i64 i64 i64 i64) (result i64)
        (call $dispatch_create
            (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4) (local.get 5) (local.get 6)))
    (memory (export "memory") 2)
    (func (export "pb_main"))
    (func (export "log") (param i32 i32) (result i32)
        local.get 0 local.get 1 call $log)
    (func (export "view") (param i32 i32) (result i32)
        local.get 0 local.get 1 call $view)
    (func (export "event") (param i32 i32 i32) (result i32)
        local.get 0 local.get 1 local.get 2 call $event)
    (func (export "identity") (param i32) (result i32)
        local.get 0 call $identity)
    (func (export "volumes") (param i32) (result i32)
        local.get 0 call $volumes)
    (func (export "create_begin") (param i32 i32 i32 i32 i32) (result i32)
        local.get 0 local.get 1 local.get 2 local.get 3 local.get 4 call $create_begin)
    (func (export "edit_begin") (param i32 i32 i32 i32 i32) (result i32)
        local.get 0 local.get 1 local.get 2 local.get 3 local.get 4 call $edit_begin)

    (func (export "defer") (param i64) (result i32)
        local.get 0 call $defer)
)"#;

#[derive(Default)]
struct Records {
    session_key_secret: Option<[u8; 16]>,
    creates: usize,
    edits: usize,
    queries: usize,
    denied: bool,
    fields: Vec<FieldChange>,
    volumes: Vec<crate::runtime::host::EntryAddress>,
    schema_reads: usize,
    calls: Vec<Arc<crate::runtime::host::DataCall>>,
    results: std::collections::BTreeMap<u64, HostResult<Arc<crate::runtime::host::DataResult>>>,
}

struct TestHost(Arc<Mutex<Records>>);

impl Host for TestHost {
    fn session_key_secret(&self) -> HostResult<Option<[u8; 16]>> {
        Ok(self.0.lock().unwrap().session_key_secret)
    }
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::SchemaReadBegin,
            Opcode::SchemaReadTake,
            Opcode::ObjectCreateBegin,
            Opcode::ObjectCreatePoll,
            Opcode::ObjectReadBegin,
            Opcode::ObjectReadTake,
            Opcode::ObjectEditBegin,
            Opcode::ObjectEditTake,
            Opcode::ObjectQueryBegin,
            Opcode::ObjectQueryTake,
        ]
    }
    fn begin_data(&mut self, _: u64, call: Arc<crate::runtime::host::DataCall>) -> HostResult<u64> {
        let mut records = self.0.lock().unwrap();
        records.calls.push(call);
        Ok(records.calls.len() as u64)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<crate::runtime::host::DataResult>> {
        self.0.lock().unwrap().results.get(&token).cloned().unwrap_or(Err(HostError::WouldBlock))
    }
    fn release_data(&mut self, token: u64) {
        self.0.lock().unwrap().results.remove(&token);
    }
    fn schema(&mut self, volume: crate::runtime::host::EntryAddress) -> HostResult<Schema> {
        assert_eq!(volume.volume_id, ROOT);
        self.0.lock().unwrap().schema_reads += 1;
        Ok(Schema {
            types: vec![TypeDeclaration {
                id: TYPE,
                name: "Counter".to_owned(),
                deprecated: false,
                fields: vec![FieldDeclaration {
                    id: FIELD,
                    name: "value".to_owned(),
                    value_type: ValueType::Integer,
                    repeated: false,
                    deprecated: false,
                }],
            }],
        })
    }
    fn establish_schema(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        _schema: &Schema,
    ) -> HostResult<()> {
        self.0.lock().unwrap().volumes.push(volume);
        Ok(())
    }
    fn create_object(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        type_id: Id,
        changes: &[FieldChange],
    ) -> HostResult<Id> {
        assert_eq!(type_id, TYPE);
        let mut records = self.0.lock().unwrap();
        records.volumes.push(volume);
        records.creates += 1;
        records.fields = changes.to_vec();
        Ok(OBJECT)
    }
    fn read_object(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        object_id: Id,
        fields: &[Id],
    ) -> HostResult<ObjectRecord> {
        assert_eq!(volume.volume_id, ROOT);
        assert_eq!(object_id, OBJECT);
        let records = self.0.lock().unwrap();
        if records.denied {
            return Err(HostError::AccessDenied);
        }
        Ok(ObjectRecord {
            id: OBJECT,
            type_id: TYPE,
            fields: fields
                .iter()
                .map(|id| {
                    records
                        .fields
                        .iter()
                        .find(|field| field.field_id == *id)
                        .cloned()
                        .unwrap_or(FieldChange { field_id: *id, values: None })
                })
                .collect(),
        })
    }
    fn edit_fields(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        object_id: Id,
        changes: &[FieldChange],
    ) -> HostResult<()> {
        assert_eq!(volume.volume_id, ROOT);
        assert_eq!(object_id, OBJECT);
        let mut records = self.0.lock().unwrap();
        if records.denied {
            return Err(HostError::AccessDenied);
        }
        records.edits += 1;
        for change in changes {
            records.fields.retain(|field| field.field_id != change.field_id);
            records.fields.push(change.clone());
        }
        Ok(())
    }
    fn query_objects(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        type_id: Id,
        _after: Option<Id>,
        _limit: u32,
        projection: Option<&[Id]>,
    ) -> HostResult<ObjectPage> {
        assert_eq!(volume.volume_id, ROOT);
        assert_eq!(type_id, TYPE);
        let mut records = self.0.lock().unwrap();
        records.queries += 1;
        Ok(ObjectPage {
            objects: if records.denied { vec![] } else { vec![OBJECT] },
            next: None,
            records: projection.map(|fields| {
                if records.denied {
                    vec![]
                } else {
                    vec![ObjectRecord {
                        id: OBJECT,
                        type_id: TYPE,
                        fields: records
                            .fields
                            .iter()
                            .filter(|field| fields.contains(&field.field_id))
                            .cloned()
                            .collect(),
                    }]
                }
            }),
        })
    }
}

fn context() -> GuestContext {
    GuestContext {
        app_name: "Test app".into(),
        app_build: None,
        identity: Identity {
            display_name: String::new(),
            account_id: [10; 16],
            is_service: false,
            device_id: [11; 16],
            instance_id: [14; 16],
            session_id: None,
        },
        // Duplicate names are valid; the root is deliberately not first.
        volumes: vec![
            AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: [8; 16],
                name: "Data".to_owned(),
                tags: vec!["personal".to_owned()],
                rights: VolumeRights::READ,
                is_root: false,
                role: 0,
            },
            AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: ROOT,
                name: "Data".to_owned(),
                tags: vec!["project".to_owned()],
                rights: VolumeRights::ALL,
                is_root: true,
                role: 0,
            },
        ],
        startup_arguments: BTreeMap::from([("mode".to_owned(), "test".to_owned())]),
        trace_catalog: None,
        required_capabilities: BTreeSet::from([
            "event.poll".to_owned(),
            "object.read.begin".to_owned(),
            "object.read.take".to_owned(),
        ]),
        participants: Vec::new(),
    }
}

#[test]
fn startup_argument_limits_apply_before_guest_instantiation_on_both_engines() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let runtime = Runtime::with_backend(backend).unwrap();
        let wasm = wat::parse_str(CALLER).unwrap();
        let mut input = context();
        input.startup_arguments =
            (0..abi::MAX_STARTUP_ARGUMENTS).map(|n| (format!("arg-{n}"), "value".into())).collect();
        let create = |input| {
            runtime.create_guest(
                &wasm,
                input,
                Box::new(TestHost(Arc::new(Mutex::new(Records::default())))),
            )
        };
        create(input.clone()).unwrap();
        input.startup_arguments.insert("extra".into(), "value".into());
        assert!(create(input.clone()).is_err());
        for name in ["Mode", "bad name", "é", "a=b"] {
            input.startup_arguments = [(name.into(), "value".into())].into();
            assert!(create(input.clone()).is_err());
        }
        input.startup_arguments = [("text".into(), "é".repeat(2048))].into();
        create(input.clone()).unwrap();
        input.startup_arguments.get_mut("text").unwrap().push('x');
        assert!(create(input.clone()).is_err());
        input.startup_arguments =
            [("a".into(), "\0".repeat(4096)), ("b".into(), "\0".repeat(4096))].into();
        assert!(create(input).is_err(), "JSON escaped bytes must fit the shared launch budget");
    }
}

#[test]
fn a_volume_list_that_cannot_be_returned_is_refused_before_the_guest_starts() {
    let runtime = Runtime::new().unwrap();
    let wasm = wat::parse_str(CALLER).unwrap();
    let create = |input| {
        runtime.create_guest(
            &wasm,
            input,
            Box::new(TestHost(Arc::new(Mutex::new(Records::default())))),
        )
    };
    // Every volume within its own limits, and the whole list beyond what
    // one AvailableVolumes response can carry. Starting this guest would
    // leave it unable to obtain any volume handle, because the only other
    // source of one is a native file-selection dialog.
    let mut input = context();
    let volumes = |tags: usize| {
        (0..abi::MAX_VOLUMES)
            .map(|n| AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: [n as u8; 16],
                name: "v".repeat(256),
                tags: (0..tags).map(|_| "t".repeat(128)).collect(),
                rights: VolumeRights::READ,
                is_root: n == 0,
                role: 0,
            })
            .collect::<Vec<_>>()
    };
    input.volumes = volumes(64);
    assert!(create(input.clone()).is_err());
    // The same count without tags fits, and still starts.
    input.volumes = volumes(0);
    create(input).unwrap();
}

#[test]
fn semantic_input_rejects_invalid_payloads_and_disabled_menu_ancestors() {
    use crate::runtime::{MenuEntry, ViewNode};
    let action = MenuEntry::Action {
        icon: None,
        id: 3,
        label: "Action".into(),
        checked: None,
        disabled: false,
        destructive: false,
    };
    let view = ViewNode::Column(vec![
        ViewNode::Button {
            id: 1,
            label: "Press".into(),
            style: playbit_api::ui::ButtonStyle::Default,
            disabled: false,
        },
        ViewNode::TextField {
            id: 2,
            value: String::new(),
            placeholder: String::new(),
            multiline: false,
            disabled: false,
            read_only: false,
        },
        ViewNode::PopupButton {
            id: 4,
            label: "Menu".into(),
            disabled: false,
            items: vec![MenuEntry::Submenu {
                icon: None,
                label: "Disabled".into(),
                disabled: true,
                items: vec![action.clone()],
            }],
        },
    ]);
    let click = Event { kind: EventKind::Click, target: 1, data: Vec::new() };
    assert_eq!(validate_input(&view, &click), Ok(()));
    assert_eq!(
        validate_input(&view, &Event { data: vec![1], ..click.clone() }),
        Err(HostError::InvalidArgument)
    );
    assert_eq!(
        validate_input(&view, &Event { target: 3, ..click.clone() }),
        Err(HostError::InvalidArgument)
    );
    assert_eq!(
        validate_input(&view, &Event { target: 4, ..click.clone() }),
        Err(HostError::InvalidArgument)
    );
    assert_eq!(
        validate_input(
            &view,
            &Event { kind: EventKind::Submit, target: 2, data: "Résumé".as_bytes().to_vec() }
        ),
        Ok(())
    );
    assert_eq!(
        validate_input(&view, &Event { kind: EventKind::Submit, target: 2, data: vec![255] }),
        Err(HostError::InvalidArgument)
    );
    assert_eq!(
        validate_input(
            &view,
            &Event {
                kind: EventKind::Submit,
                target: 2,
                data: vec![b'a'; abi::MAX_EVENT_BYTES + 1]
            }
        ),
        Err(HostError::LimitExceeded)
    );
    assert_eq!(
        validate_input(&view, &Event { kind: EventKind::DataChanged, ..click.clone() }),
        Err(HostError::InvalidArgument)
    );
    assert_eq!(
        validate_input(
            &ViewNode::ContextMenu { items: vec![action], child: Box::new(view) },
            &Event { target: 3, ..click }
        ),
        Ok(())
    );
}

#[test]
fn duplicate_ids_in_controls_or_nested_menus_reject_submission_on_both_backends() {
    use playbit_api::ui::{ButtonStyle, MenuItem};
    for menu_duplicate in [false, true] {
        let mut encoder_state = playbit_api::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        encoder.button(1, "First", ButtonStyle::Default, false);
        if menu_duplicate {
            encoder.popup_button(
                2,
                "Menu",
                &[MenuItem::Submenu {
                    icon: None,
                    label: "Nested",
                    disabled: false,
                    items: &[MenuItem::Action {
                        icon: None,
                        id: 1,
                        label: "Duplicate",
                        checked: None,
                        disabled: false,
                        destructive: false,
                    }],
                }],
                false,
            );
        } else {
            encoder.button(1, "Duplicate", ButtonStyle::Default, true);
        }
        encoder.column_end();
        let view = encoder.finish();
        assert!(decode_view(&view).is_err());
        for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
            let (mut guest, _) = guest(backend);
            assert_eq!(submit_view(&mut guest, &view), HostError::InvalidArgument as i32);
            assert!(guest.store.data().app.view.is_none());
        }
    }
}

#[test]
fn malicious_scene_counts_fail_before_native_allocation_on_both_backends() {
    use playbit_api::ui::*;
    fn command(bytes: &mut Vec<u8>, kind: u8, id: u32, payload: &[u8]) {
        bytes.push(kind);
        bytes.extend_from_slice(&id.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
    }
    let mut autocomplete = vec![0; 9]; // flags, value length, placeholder length
    autocomplete.extend_from_slice(&u32::MAX.to_le_bytes());
    let mut popup = vec![0; 5]; // flags, label length
    popup.extend_from_slice(&u32::MAX.to_le_bytes());
    let mut nested_menu = vec![0; 5];
    nested_menu.extend_from_slice(&1u32.to_le_bytes());
    nested_menu.extend_from_slice(&[MENU_SUBMENU, 0]);
    nested_menu.extend_from_slice(&0u32.to_le_bytes());
    nested_menu.extend_from_slice(&u32::MAX.to_le_bytes());
    for (kind, payload) in [
        (VIEW_AUTOCOMPLETE_FIELD, autocomplete),
        (VIEW_POPUP_BUTTON, popup),
        (VIEW_POPUP_BUTTON, nested_menu),
    ] {
        let mut view = VIEW_MAGIC.to_vec();
        command(&mut view, VIEW_COLUMN_START, 0, &[]);
        command(&mut view, kind, 1, &payload);
        command(&mut view, VIEW_COLUMN_END, 0, &[]);
        for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
            let (mut guest, _) = guest(backend);
            assert_eq!(submit_view(&mut guest, &view), HostError::InvalidArgument as i32);
        }
    }
}

#[derive(Default)]
struct ImageFileData {
    bytes: Vec<u8>,
    denied: bool,
    tampered: bool,
    revoke_after_last_chunk: bool,
    replace_after_last_chunk: bool,
    short_read: bool,
    read_calls: usize,
}
struct ImageFileHost(Arc<Mutex<ImageFileData>>);
impl Host for ImageFileHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::ObjectReadBegin,
            Opcode::ObjectReadTake,
            Opcode::FileReadBegin,
            Opcode::FileReadTake,
        ]
    }
    fn read_object(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        object_id: Id,
        fields: &[Id],
    ) -> HostResult<ObjectRecord> {
        assert_eq!((volume.volume_id, object_id, fields), (ROOT, OBJECT, [FIELD].as_slice()));
        let data = self.0.lock().unwrap();
        if data.denied {
            return Err(HostError::AccessDenied);
        }
        Ok(ObjectRecord {
            id: OBJECT,
            type_id: TYPE,
            fields: vec![FieldChange {
                field_id: FIELD,
                values: Some(vec![Value::File { volume_id: ROOT, entry_id: [7; 15] }]),
            }],
        })
    }
    fn file_metadata(
        &mut self,
        address: crate::runtime::host::EntryAddress,
    ) -> HostResult<(u64, [u8; 32])> {
        assert_eq!(
            address,
            crate::runtime::host::EntryAddress { volume_id: ROOT, entry_id: [7; 15] }
        );
        let data = self.0.lock().unwrap();
        if data.denied {
            return Err(HostError::AccessDenied);
        }
        Ok((data.bytes.len() as u64, *blake3::hash(&data.bytes).as_bytes()))
    }
    fn read_file(
        &mut self,
        address: crate::runtime::host::EntryAddress,
        offset: u64,
        limit: u32,
    ) -> HostResult<FileChunk> {
        assert_eq!(
            address,
            crate::runtime::host::EntryAddress { volume_id: ROOT, entry_id: [7; 15] }
        );
        let mut data = self.0.lock().unwrap();
        if data.denied {
            return Err(HostError::AccessDenied);
        }
        if limit == 0 || limit > playbit_api::abi::files::MAX_FILE_READ {
            return Err(HostError::InvalidArgument);
        }
        data.read_calls += 1;
        let end = (offset as usize + limit as usize).min(data.bytes.len());
        let mut bytes = data.bytes[offset as usize..end].to_vec();
        if data.tampered && !bytes.is_empty() {
            bytes[0] ^= 1;
        }
        if data.short_read {
            bytes.clear();
        }
        if data.replace_after_last_chunk && end == data.bytes.len() && !data.bytes.is_empty() {
            data.bytes[0] ^= 2;
        }
        if data.revoke_after_last_chunk && end == data.bytes.len() {
            data.denied = true;
        }
        Ok(FileChunk { total_bytes: data.bytes.len() as u64, bytes })
    }
}

fn file_image_guest() -> Vec<u8> {
    let mut encoder_state = playbit_api::context::ContextState::default();
    let mut encoder = encoder_state.view();
    encoder.column_start();
    encoder.scroll_start(91, true);
    encoder.file_image(
        playbit_api::ui::FileReference {
            volume: VolumeHandle { generation: 1, slot: 2 },
            entry_id: [7; 15],
        },
        320,
        240,
        "Test image",
    );
    encoder.scroll_end();
    encoder.column_end();
    let bytes = encoder.finish();
    let encoded: String = bytes.iter().map(|byte| format!("\\{byte:02x}")).collect();
    // The fixture declares the second initial capability. Its slot generation
    // is independent of the guest epoch used by presentation callbacks.
    wat::parse_str(format!(
        r#"(module
        (import "playbit" "pb_syscall" (func $dispatch_view
            (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (func $view (param i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_view (i32.const 3)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.const 0)
                (i64.const 0) (i64.const 0) (i64.const 0))))
        (memory (export "memory") 1)
        (data (i32.const 1024) "{encoded}")
        (func (export "pb_main")
            (if (i32.lt_s (call $view (i32.const 1024) (i32.const {})) (i32.const 0))
                (then unreachable))))"#,
        bytes.len()
    ))
    .unwrap()
}

#[test]
fn native_file_presentation_rechecks_scene_generation_hash_and_authority() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let data =
            Arc::new(Mutex::new(ImageFileData { bytes: vec![17; 100_001], ..Default::default() }));
        let mut guest = crate::runtime::managed::ManagedGuest::create(
            Runtime::with_backend(backend).unwrap(),
            &file_image_guest(),
            context(),
            Box::new(ImageFileHost(Arc::clone(&data))),
            |_| Ok(()),
        )
        .unwrap();
        let generation = guest.generation();
        let file = playbit_api::ui::FileReference {
            volume: VolumeHandle { generation: 1, slot: 2 },
            entry_id: [7; 15],
        };
        assert_eq!(
            guest.file_descriptor(generation, file).unwrap(),
            (100_001, *blake3::hash(&data.lock().unwrap().bytes).as_bytes())
        );
        assert_eq!(data.lock().unwrap().read_calls, 0, "metadata probe must not read file chunks");
        data.lock().unwrap().denied = true;
        assert_eq!(guest.file_descriptor(generation, file), Err(HostError::AccessDenied));
        data.lock().unwrap().denied = false;
        assert_eq!(guest.file_descriptor(generation + 1, file), Err(HostError::StaleHandle));
        assert_eq!(guest.read_file(generation, file, 1024), Err(HostError::LimitExceeded));
        assert_eq!(data.lock().unwrap().read_calls, 0);
        let (digest, bytes) =
            guest.read_file(generation, file, MAX_PRESENTATION_FILE_BYTES).unwrap();
        assert_eq!(bytes, data.lock().unwrap().bytes);
        assert_eq!(digest, *blake3::hash(&bytes).as_bytes());
        assert_eq!(data.lock().unwrap().read_calls, 2);
        assert_eq!(
            guest.read_file(generation + 1, file, MAX_PRESENTATION_FILE_BYTES),
            Err(HostError::StaleHandle)
        );
        let outside_scene = playbit_api::ui::FileReference { entry_id: [99; 15], ..file };
        assert_eq!(
            guest.read_file(generation, outside_scene, MAX_PRESENTATION_FILE_BYTES),
            Err(HostError::AccessDenied)
        );
        data.lock().unwrap().tampered = true;
        assert_eq!(
            guest.read_file(generation, file, MAX_PRESENTATION_FILE_BYTES),
            Err(HostError::Internal)
        );
        data.lock().unwrap().tampered = false;
        data.lock().unwrap().short_read = true;
        assert_eq!(
            guest.read_file(generation, file, MAX_PRESENTATION_FILE_BYTES),
            Err(HostError::Conflict)
        );
        data.lock().unwrap().short_read = false;
        data.lock().unwrap().replace_after_last_chunk = true;
        assert_eq!(
            guest.read_file(generation, file, MAX_PRESENTATION_FILE_BYTES),
            Err(HostError::Conflict)
        );
        data.lock().unwrap().replace_after_last_chunk = false;
        data.lock().unwrap().revoke_after_last_chunk = true;
        assert_eq!(
            guest.read_file(generation, file, MAX_PRESENTATION_FILE_BYTES),
            Err(HostError::AccessDenied)
        );
        data.lock().unwrap().revoke_after_last_chunk = false;
        data.lock().unwrap().denied = false;
        guest
            .replace(
                &file_image_guest(),
                None,
                BTreeSet::new(),
                Box::new(ImageFileHost(Arc::clone(&data))),
            )
            .unwrap();
        assert_ne!(guest.generation(), generation);
        assert_eq!(
            guest.read_file(generation, file, MAX_PRESENTATION_FILE_BYTES),
            Err(HostError::StaleHandle)
        );
    }
}

#[test]
fn object_file_scene_requires_current_read_capabilities_and_rejects_path_images() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, _) = guest(backend);
        for opcode in [18, 19] {
            // Removed commands are invalid even when framed as valid scene nodes.
            let mut bytes = b"PBV1".to_vec();
            bytes.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 0]);
            bytes.extend_from_slice(&[opcode, 0, 0, 0, 0, 0, 0, 0, 0]);
            bytes.extend_from_slice(&[4, 0, 0, 0, 0, 0, 0, 0, 0]);
            assert!(decode_view(&bytes).is_err());
            assert_eq!(submit_view(&mut guest, &bytes), HostError::InvalidArgument as i32);
        }
        let file = playbit_api::ui::FileReference {
            volume: guest.volume_handle(ROOT).unwrap(),
            entry_id: [7; 15],
        };
        let mut encoder_state = playbit_api::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        encoder.file_image(file, 320, 240, "Test image");
        encoder.column_end();
        let encoded = encoder.finish();
        for end in 0..encoded.len() {
            assert!(decode_view(&encoded[..end]).is_err(), "accepted truncated image at {end}");
        }
        for (start, end) in [(22, 26), (26, 30), (30, 45), (45, 49), (49, 53)] {
            let mut malformed = encoded.to_vec();
            malformed[start..end].fill(0);
            assert!(decode_view(&malformed).is_err(), "accepted zero field at {start}");
        }
        for (offset, value) in [(14, 1u32), (18, 30), (18, 32), (49, 4097), (53, u32::MAX)] {
            let mut malformed = encoded.to_vec();
            malformed[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(decode_view(&malformed).is_err(), "accepted malformed image field at {offset}");
        }
        let scene = decode_view(&encoded).unwrap();
        assert_eq!(
            scene,
            crate::runtime::ViewNode::Theme {
                theme: playbit_api::ui::Theme::light(),
                window_background_specified: false,
                window_style: None,
                window_size_constraints: None,
                child: Box::new(crate::runtime::ViewNode::Column(vec![
                    crate::runtime::ViewNode::FileImage {
                        file,
                        width: 320,
                        height: 240,
                        label: Some("Test image".into()),
                    }
                ]))
            }
        );
        assert_eq!(submit_view(&mut guest, &encoded), HostError::Unsupported as i32);
        guest.store.data_mut().app.supported.insert(Opcode::FileReadBegin);
        guest.store.data_mut().app.supported.insert(Opcode::FileReadTake);
        super::test_support::set_volume_rights(
            &mut guest.store.data_mut().app,
            1,
            VolumeRights::WRITE,
        );
        assert_eq!(submit_view(&mut guest, &encoded), HostError::AccessDenied as i32);
        super::test_support::set_volume_rights(
            &mut guest.store.data_mut().app,
            1,
            VolumeRights::ALL,
        );
        assert_eq!(submit_view(&mut guest, &encoded), 0);
        // Established image20 still runs on both Native and Pulley. Annotation
        // changes presentation only; it does not introduce a new file grant.
        let mut primitive = encoded.to_vec();
        primitive.drain(53..72);
        assert_eq!(submit_view(&mut guest, &primitive), 0);
        let mut malformed_label = encoded.to_vec();
        malformed_label[62] = 0xff;
        assert_eq!(submit_view(&mut guest, &malformed_label), HostError::InvalidArgument as i32);
        let mut malformed = encoded.to_vec();
        malformed[26..30].copy_from_slice(&(file.volume.generation + 1).to_le_bytes());
        assert_eq!(submit_view(&mut guest, &malformed), HostError::StaleHandle as i32);
        malformed = encoded.to_vec();
        malformed[45..49].copy_from_slice(&0u32.to_le_bytes()); // width
        assert_eq!(submit_view(&mut guest, &malformed), HostError::InvalidArgument as i32);
    }
}

fn submit_view(guest: &mut GuestInstance, view: &[u8]) -> i32 {
    write(guest, 8, view);
    call(guest, "view", (8u32, view.len() as u32))
}

fn guest(backend: ExecutionBackend) -> (GuestInstance, Arc<Mutex<Records>>) {
    let records = Arc::new(Mutex::new(Records::default()));
    let guest = Runtime::with_backend(backend)
        .unwrap()
        .create_guest(
            &wat::parse_str(CALLER).unwrap(),
            context(),
            Box::new(TestHost(Arc::clone(&records))),
        )
        .unwrap();
    (guest, records)
}

fn put_u32(guest: &mut GuestInstance, offset: usize, value: u32) {
    write(guest, offset, &value.to_le_bytes());
}
fn get_u32(guest: &mut GuestInstance, offset: usize) -> u32 {
    u32::from_le_bytes(read(guest, offset, 4).try_into().unwrap())
}
fn stage_records(guest: &mut GuestInstance, pointer: usize, count: usize, size: usize) {
    for index in 0..count {
        write(guest, pointer + index * size, &vec![0; size]);
        put_u32(guest, pointer + index * size, size as u32);
    }
}
fn words(guest: &mut GuestInstance, pointer: usize, values: &[u32]) {
    for (index, value) in values.iter().enumerate() {
        put_u32(guest, pointer + index * 4, *value);
    }
}
// Real wasm32 C records, with separate arrays and nonnull input IDs.
fn stage_changes(guest: &mut GuestInstance, changes: &[FieldChange]) {
    write(guest, 16, &TYPE);
    write(guest, 32, &OBJECT);
    write(guest, 48, &FIELD);
    let mut value_offset = 8192usize;
    let mut byte_offset = 32768usize;
    for (index, field) in changes.iter().enumerate() {
        let offset = 1024 + index * 40;
        write(guest, offset, &[0; 40]);
        put_u32(guest, offset, 40);
        write(guest, offset + 8, &field.field_id);
        put_u32(guest, offset + 24, value_offset as u32);
        put_u32(guest, offset + 28, field.values.as_ref().map_or(0, Vec::len) as u32);
        put_u32(guest, offset + 32, 80);
        put_u32(guest, offset + 36, u32::from(field.values.is_some()));
        for value in field.values.iter().flatten() {
            use playbit_api::abi::data as tag;
            let mut record = [0u8; 80];
            let kind = match value {
                Value::Text(text) => {
                    record[12..16].copy_from_slice(&(text.len() as u32).to_le_bytes());
                    record[72..76].copy_from_slice(&(byte_offset as u32).to_le_bytes());
                    write(guest, byte_offset, text.as_bytes());
                    byte_offset += text.len();
                    tag::TEXT
                }
                Value::Bytes(bytes) => {
                    record[12..16].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
                    record[72..76].copy_from_slice(&(byte_offset as u32).to_le_bytes());
                    write(guest, byte_offset, bytes);
                    byte_offset += bytes.len();
                    tag::BYTES
                }
                Value::Boolean(value) => {
                    record[16..24].copy_from_slice(&u64::from(*value).to_le_bytes());
                    tag::BOOLEAN
                }
                Value::Integer(value) => {
                    record[16..24].copy_from_slice(&value.to_le_bytes());
                    tag::INTEGER
                }
                Value::Float(value) => {
                    record[16..24].copy_from_slice(&value.to_le_bytes());
                    tag::FLOAT
                }
                Value::Reference { volume_id, database_id, object_id } => {
                    record[24..40].copy_from_slice(volume_id);
                    record[40..56].copy_from_slice(object_id);
                    record[56..71].copy_from_slice(database_id);
                    tag::REFERENCE
                }
                Value::File { volume_id, entry_id } => {
                    record[24..40].copy_from_slice(volume_id);
                    record[56..71].copy_from_slice(entry_id);
                    tag::FILE
                }
            };
            record[..4].copy_from_slice(&80u32.to_le_bytes());
            record[8..12].copy_from_slice(&kind.to_le_bytes());
            write(guest, value_offset, &record);
            value_offset += 80;
        }
    }
}
const DATA_HANDLE: usize = 98304;
fn scalar(guest: &mut GuestInstance, opcode: Opcode, args: [u64; 6]) -> i32 {
    super::test_support::invoke(
        guest,
        "invoke",
        (opcode as u32, args[0], args[1], args[2], args[3], args[4], args[5]),
    )
}
fn execute_guest(
    guest: &mut GuestInstance,
    records: &Arc<Mutex<Records>>,
    pending: OperationHandle,
) {
    let (token, owned) = match guest
        .store
        .data()
        .app
        .capabilities
        .operation(crate::capability::Handle::new(pending.generation, pending.slot))
        .unwrap()
    {
        PendingOperation::Data { token, call, .. } => (*token, call.as_ref().clone()),
        _ => panic!("data operation"),
    };
    // This driver executes between exports, never in a guest syscall.
    let result = owned.execute(&mut TestHost(records.clone())).map(Arc::new);
    assert!(records.lock().unwrap().results.insert(token, result).is_none());
}
fn packed(handle: OperationHandle) -> u64 {
    playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot)
}
fn begin_query(
    guest: &mut GuestInstance,
    volume: VolumeHandle,
    input: u32,
) -> Result<OperationHandle, i32> {
    let (entry, _) = super::test_support::entry_pointers(guest, volume);
    let status = scalar(
        guest,
        Opcode::ObjectQueryBegin,
        [u64::from(entry), u64::from(input), DATA_HANDLE as u64, 0, 0, 0],
    );
    if status < 0 {
        return Err(status);
    }
    assert_eq!(status, 0);
    Ok(super::test_support::operation_handle(guest, DATA_HANDLE))
}
fn query(
    guest: &mut GuestInstance,
    records: &Arc<Mutex<Records>>,
    volume: VolumeHandle,
    input: u32,
    output: u32,
) -> i32 {
    let pending = match begin_query(guest, volume, input) {
        Ok(handle) => handle,
        Err(status) => return status,
    };
    execute_guest(guest, records, pending);
    scalar(guest, Opcode::ObjectQueryTake, [packed(pending), u64::from(output), 0, 0, 0, 0])
}

fn read_object(
    guest: &mut GuestInstance,
    handle: VolumeHandle,
    records: &Arc<Mutex<Records>>,
) -> i32 {
    write(guest, 32, &OBJECT);
    write(guest, 48, &FIELD);
    words(guest, 64, &[68, 0, 0, 0, 0, 0, 1024, 16, 0, 40, 8192, 16, 0, 80, 32768, 1024, 0]);
    stage_records(guest, 1024, 16, 40);
    stage_records(guest, 8192, 16, 80);
    let (entry, _) = super::test_support::entry_pointers(guest, handle);
    let status = scalar(
        guest,
        Opcode::ObjectReadBegin,
        [u64::from(entry), 32, 48, 1, DATA_HANDLE as u64, 0],
    );
    if status < 0 {
        return status;
    }
    let pending = super::test_support::operation_handle(guest, DATA_HANDLE);
    execute_guest(guest, records, pending);
    scalar(guest, Opcode::ObjectReadTake, [packed(pending), 64, 0, 0, 0, 0])
}

#[test]
fn query_projection_extends_ids_and_retains_its_short_result() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        records.lock().unwrap().fields = vec![
            FieldChange { field_id: FIELD, values: Some(vec![Value::Text("hello".into())]) },
            FieldChange { field_id: [4; 16], values: None },
        ];
        let handle = guest.volume_handle(ROOT).unwrap();
        let (_entry, _stale_entry) = super::test_support::entry_pointers(&mut guest, handle);
        words(&mut guest, 64, &[60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 64, 0, 0, 0]);
        write(&mut guest, 72, &TYPE);
        words(
            &mut guest,
            128,
            &[92, 0, 0, 0, 0, 0, 0, 0, 1024, 1, 0, 48, 2048, 2, 0, 40, 4096, 1, 0, 80, 8192, 5, 0],
        );
        stage_records(&mut guest, 1024, 1, 48);
        stage_records(&mut guest, 2048, 2, 40);
        stage_records(&mut guest, 4096, 1, 80);
        assert_eq!(query(&mut guest, &records, handle, 64, 128), 0);
        assert_eq!(get_u32(&mut guest, 168), 1);
        assert_eq!(read(&mut guest, 1032, 16), OBJECT);
        assert_eq!(get_u32(&mut guest, 156), 0);
        assert_eq!(get_u32(&mut guest, 1068), 0);
        // Request projection, then retry a byte buffer that is one byte short.
        put_u32(&mut guest, 104, 2);
        put_u32(&mut guest, 112, 256);
        put_u32(&mut guest, 116, 2);
        write(&mut guest, 256, &[FIELD, [4; 16]].concat());
        put_u32(&mut guest, 212, 4);
        let pending = begin_query(&mut guest, handle, 64).unwrap();
        execute_guest(&mut guest, &records, pending);
        let before = read(&mut guest, 0, 131072);
        assert_eq!(
            scalar(&mut guest, Opcode::ObjectQueryTake, [packed(pending), 128, 0, 0, 0, 0]),
            HostError::BufferTooSmall as i32
        );
        assert_eq!(
            [
                get_u32(&mut guest, 168),
                get_u32(&mut guest, 184),
                get_u32(&mut guest, 200),
                get_u32(&mut guest, 216)
            ],
            [1, 2, 1, 5]
        );
        let mut after = read(&mut guest, 0, 131072);
        for offset in [132, 168, 184, 200, 216, 1028, 2052, 2092, 4100] {
            after[offset..offset + 4].copy_from_slice(&before[offset..offset + 4]);
        }
        assert!(
            after == before,
            "unexpected memory write at {:?}",
            after.iter().zip(&before).position(|(a, b)| a != b)
        );
        put_u32(&mut guest, 212, 5);
        assert_eq!(
            scalar(&mut guest, Opcode::ObjectQueryTake, [packed(pending), 128, 0, 0, 0, 0]),
            0
        );
        assert_eq!(get_u32(&mut guest, 156), 1);
        assert_eq!(get_u32(&mut guest, 1068), 2);
        assert_eq!(read(&mut guest, 1048, 16), TYPE);
        assert_eq!(read(&mut guest, 2056, 16), FIELD);
        assert_eq!(get_u32(&mut guest, 2080), 1);
        assert_eq!(read(&mut guest, 2096, 16), [4; 16]);
        assert_eq!(get_u32(&mut guest, 2120), 0);
        assert_eq!(get_u32(&mut guest, 4104), playbit_api::abi::data::TEXT);
        assert_eq!(get_u32(&mut guest, 4152), 0);
        assert_eq!(get_u32(&mut guest, 4172), 5);
        assert_eq!(read(&mut guest, 8192, 5), b"hello");
        records.lock().unwrap().denied = true;
        assert_eq!(query(&mut guest, &records, handle, 64, 128), 0);
        assert_eq!(
            [
                get_u32(&mut guest, 168),
                get_u32(&mut guest, 184),
                get_u32(&mut guest, 200),
                get_u32(&mut guest, 216)
            ],
            [0; 4]
        );
        assert_eq!(records.lock().unwrap().queries, 3);
        for (flags, pointer, count) in
            [(2u32, 256u32, 0u32), (2, 0, 2), (2, 131056, 2), (0, 256, 2)]
        {
            put_u32(&mut guest, 104, flags);
            put_u32(&mut guest, 112, pointer);
            put_u32(&mut guest, 116, count);
            assert_eq!(
                query(&mut guest, &records, handle, 64, 128),
                HostError::InvalidArgument as i32
            );
        }
        put_u32(&mut guest, 104, 2);
        put_u32(&mut guest, 112, 256);
        put_u32(&mut guest, 116, 2);
        write(&mut guest, 272, &FIELD);
        assert_eq!(query(&mut guest, &records, handle, 64, 128), HostError::InvalidArgument as i32);
        assert_eq!(records.lock().unwrap().queries, 3, "invalid projection never reaches host");
    }
}

#[test]
fn projected_query_rejects_host_identity_type_field_and_value_violations() {
    let record = ObjectRecord {
        id: OBJECT,
        type_id: TYPE,
        fields: vec![FieldChange { field_id: FIELD, values: Some(vec![Value::Text("ok".into())]) }],
    };
    let mut page =
        ObjectPage { objects: vec![OBJECT], next: None, records: Some(vec![record.clone()]) };
    let validate = |page: &ObjectPage| {
        data_validation::query_page(
            page,
            TYPE,
            None,
            64,
            Some(&[FIELD]),
            abi::MAX_DATA_BYTES as u32,
        )
    };
    validate(&page).unwrap();
    page.records.as_mut().unwrap()[0].id = [99; 16];
    assert_eq!(validate(&page), Err(HostError::Internal));
    page.records = Some(vec![record.clone()]);
    page.records.as_mut().unwrap()[0].type_id = [99; 16];
    assert_eq!(validate(&page), Err(HostError::Internal));
    page.records = Some(vec![record.clone()]);
    page.records.as_mut().unwrap()[0].fields[0].field_id = [99; 16];
    assert_eq!(validate(&page), Err(HostError::Internal));
    page.records = Some(vec![record.clone()]);
    page.records.as_mut().unwrap()[0].fields.push(record.fields[0].clone());
    assert_eq!(validate(&page), Err(HostError::Internal));
    page.records = Some(vec![record]);
    page.records.as_mut().unwrap()[0].fields[0].values = Some(vec![Value::Float(f64::NAN)]);
    assert_eq!(validate(&page), Err(HostError::Internal));
    page.records.as_mut().unwrap()[0].fields[0].values =
        Some(vec![Value::Bytes(vec![0; abi::MAX_DATA_BYTES + 1])]);
    assert_eq!(validate(&page), Err(HostError::LimitExceeded));
}

#[test]
fn session_info_returns_only_the_current_session_and_admits_the_complete_output_first() {
    use playbit_api::abi::session::HAS_KEY_SECRET;
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let read_info =
            |guest: &mut GuestInstance| scalar(guest, Opcode::SessionInfo, [64, 0, 0, 0, 0, 0]);
        records.lock().unwrap().session_key_secret = Some([42; 16]);
        write(&mut guest, 64, &[0xa5; 72]);
        put_u32(&mut guest, 64, 72);
        assert_eq!(read_info(&mut guest), HostError::NotFound as i32);
        assert_eq!(get_u32(&mut guest, 68), 0);
        guest.store.data_mut().app.context.identity.session_id = Some([31; 32]);
        for key_secret in [None, Some([0; 16]), Some([42; 16])] {
            records.lock().unwrap().session_key_secret = key_secret;
            assert_eq!(read_info(&mut guest), 0);
            assert_eq!(get_u32(&mut guest, 68), 64);
            assert_eq!(
                get_u32(&mut guest, 72),
                if key_secret.is_some() { HAS_KEY_SECRET } else { 0 }
            );
            assert_eq!(get_u32(&mut guest, 76), 0);
            assert_eq!(read(&mut guest, 80, 32), [31; 32]);
            assert_eq!(read(&mut guest, 112, 16), key_secret.unwrap_or([0; 16]));
            assert_eq!(read(&mut guest, 128, 8), [0xa5; 8]);
        }
        write(&mut guest, 64, &[0xa5; 72]);
        put_u32(&mut guest, 64, 63);
        let before = read(&mut guest, 64, 72);
        assert_eq!(read_info(&mut guest), HostError::BufferTooSmall as i32);
        assert_eq!(read(&mut guest, 64, 72), before);
        assert_eq!(
            scalar(&mut guest, Opcode::SessionInfo, [64, 1, 0, 0, 0, 0]),
            HostError::InvalidArgument as i32
        );
        assert_eq!(read(&mut guest, 64, 72), before);
    }
}

#[test]
fn identity_records_preserve_principals_and_optional_session() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        for session in [None, Some([42; 32])] {
            guest.store.data_mut().app.context.identity.session_id = session;
            for service in [false, true] {
                guest.store.data_mut().app.context.identity.is_service = service;
                write(&mut guest, 64, &[0xa5; 128]);
                words(&mut guest, 64, &[104, 0]);
                assert_eq!(call(&mut guest, "identity", 64u32), 0);
                let bytes = read(&mut guest, 64, 128);
                assert_eq!(&bytes[8..16], &guest.generation().to_le_bytes());
                assert_eq!(&bytes[16..32], &[10; 16]);
                assert_eq!(&bytes[32..48], &[11; 16]);
                assert_eq!(&bytes[48..64], &[14; 16]);
                assert_eq!(&bytes[64..96], &session.unwrap_or([0; 32]));
                assert_eq!(
                    &bytes[96..100],
                    &(u32::from(service) | (u32::from(session.is_some()) << 1)).to_le_bytes()
                );
                assert_eq!(&bytes[100..104], &[0; 4]);
                assert_eq!(&bytes[104..], &[0xa5; 24]);
                for pointer in [0u32, 65, 131000] {
                    if pointer == 131000 {
                        words(&mut guest, pointer as usize, &[104, 0]);
                    }
                    let before = read(&mut guest, 0, 131072);
                    assert_eq!(
                        call(&mut guest, "identity", pointer),
                        HostError::InvalidArgument as i32
                    );
                    assert_eq!(read(&mut guest, 0, 131072), before);
                }
            }
        }
        let records = records.lock().unwrap();
        assert_eq!((records.queries, records.creates, records.edits), (0, 0, 0));
        assert!(records.volumes.is_empty());
    }
}

#[test]
#[ignore = "reference-machine identity syscall timing"]
fn benchmark_identity() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, _) = guest(backend);
        let call = guest.instance.get_typed_func::<u32, i32>(&mut guest.store, "identity").unwrap();
        words(&mut guest, 64, &[104, 0]);
        let mut lookup = Vec::with_capacity(1000);
        let mut imported = Vec::with_capacity(1000);
        for n in 0..1100 {
            let start = Instant::now();
            let identity =
                environment::identity(std::hint::black_box(&guest.store.data().app)).unwrap();
            std::hint::black_box(identity);
            let elapsed = start.elapsed().as_nanos();
            if n >= 100 {
                lookup.push(elapsed);
            }
            let start = Instant::now();
            let status = call.call(&mut guest.store, 64u32).unwrap();
            let elapsed = start.elapsed().as_nanos();
            assert_eq!(status, 0);
            if n >= 100 {
                imported.push(elapsed);
            }
        }
        lookup.sort_unstable();
        imported.sort_unstable();
        println!(
            "{backend:?} identity 1000 calls, ns p50/p99/max: lookup {}/{}/{}, import {}/{}/{}",
            lookup[499], lookup[989], lookup[999], imported[499], imported[989], imported[999]
        );
        assert!(lookup[989] < 2_000);
        assert!(imported[989] < 10_000);
    }
}

#[test]
fn real_conformance_guest_runs_on_native_and_pulley() {
    let wasm = wat::parse_str(include_str!("../../../tests/fixtures/v1-conformance.wat")).unwrap();
    let mut views = Vec::new();
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let runtime = Runtime::with_backend(backend).unwrap();
        let records = Arc::new(Mutex::new(Records::default()));
        let mut guest = runtime
            .create_guest(&wasm, context(), Box::new(TestHost(Arc::clone(&records))))
            .unwrap();
        let frame = guest.pump().unwrap();
        assert!(frame.logs().is_empty());
        assert!(frame.view().is_none());
        let mut frame = frame;
        for completed in 0..5 {
            let (token, owned) = {
                let recorded = records.lock().unwrap();
                assert_eq!(recorded.calls.len(), completed + 1);
                (recorded.calls.len() as u64, recorded.calls.last().unwrap().as_ref().clone())
            };
            let creation =
                matches!(owned.operation, crate::runtime::host::DataOperation::Create { .. });
            let result = owned.execute(&mut TestHost(records.clone())).map(Arc::new);
            assert!(records.lock().unwrap().results.insert(token, result).is_none());
            if creation {
                guest.notify_creation(guest.generation(), token).unwrap();
            } else {
                guest.notify_data(guest.generation(), token).unwrap();
            }
            frame = guest.pump().unwrap();
            if completed < 4 {
                assert!(frame.view().is_none());
                assert!(frame.logs().is_empty());
            }
        }
        assert_eq!(frame.logs(), ["Hello"]);
        assert!(frame.view().is_some());
        views.push(frame.into_view());
        let stored = records.lock().unwrap();
        assert_eq!(stored.creates, 1);
        assert_eq!(stored.edits, 1);
        assert_eq!(stored.volumes, [EntryAddress { volume_id: ROOT, entry_id: [5; 15] }]);
        assert_eq!(
            stored.fields,
            [FieldChange { field_id: FIELD, values: Some(vec![Value::Integer(42)]) }]
        );
        drop(stored);
        let generation = guest.generation();
        guest
            .queue_event(
                generation,
                Event { kind: EventKind::Click, target: 7, data: b"Clicked".to_vec() },
            )
            .unwrap();
        assert_eq!(guest.pump().unwrap().logs(), ["Clicked"]);
        assert!(guest.pump().unwrap().logs().is_empty());
    }
    assert_eq!(views[0], views[1]);
}

#[test]
fn creation_initial_fields_are_validated_before_one_host_mutation() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let handle = guest.volume_handle(ROOT).unwrap();
        let (_entry, _stale_entry) = super::test_support::entry_pointers(&mut guest, handle);
        let field = FieldChange { field_id: FIELD, values: Some(vec![Value::Integer(42)]) };
        stage_changes(&mut guest, &[field.clone(), field.clone()]);
        for (pointer, count, output) in
            [(0u32, 1u32, 65536u32), (131056, 1, 65536), (1024, 2, 65536)]
        {
            let before = read(&mut guest, 0, 131072);
            assert_eq!(
                call(&mut guest, "create_begin", (_entry, 16u32, pointer, count, output)),
                HostError::InvalidArgument as i32
            );
            assert_eq!(read(&mut guest, 0, 131072), before);
            assert_eq!(records.lock().unwrap().creates, 0);
        }
        assert_eq!(call(&mut guest, "create_begin", (_entry, 16u32, 1024u32, 1u32, 65536u32)), 0);
        let pending = super::test_support::operation_handle(&mut guest, 65536);
        assert_eq!(
            scalar(&mut guest, Opcode::ObjectCreatePoll, [packed(pending), 65552, 0, 0, 0, 0]),
            -6
        );
        assert_eq!(records.lock().unwrap().creates, 0);
        execute_guest(&mut guest, &records, pending);
        assert_eq!(
            scalar(&mut guest, Opcode::ObjectCreatePoll, [packed(pending), 65552, 0, 0, 0, 0]),
            0
        );
        assert_eq!(read(&mut guest, 65552, 16), OBJECT);
        let records = records.lock().unwrap();
        assert_eq!(records.creates, 1);
        assert_eq!(records.fields, [field]);
    }
}

#[test]
fn text_changed_is_bounded_committed_utf8_for_enabled_editors_only() {
    use crate::runtime::ViewNode;
    let mut editor = ViewNode::TextField {
        id: 2,
        value: String::new(),
        placeholder: String::new(),
        multiline: false,
        disabled: false,
        read_only: false,
    };
    let mut event = Event {
        kind: EventKind::TextChanged,
        target: 2,
        data: playbit_api::abi::text::TextInput::encode("Résumé 👋", &[]).expect("fixture edit"),
    };
    assert_eq!(validate_input(&editor, &event), Ok(()));
    let mut submitted = Event {
        kind: EventKind::Submit,
        target: 2,
        data: vec![b'a'; playbit_api::abi::text::MAX_TEXT_BYTES],
    };
    assert_eq!(validate_input(&editor, &submitted), Ok(()));
    submitted.data.push(b'a');
    assert_eq!(validate_input(&editor, &submitted), Err(HostError::LimitExceeded));
    event.data = playbit_api::abi::text::TextInput::encode(
        &"a".repeat(playbit_api::abi::text::MAX_TEXT_BYTES),
        &[],
    )
    .expect("maximum text");
    assert_eq!(validate_input(&editor, &event), Ok(()));
    event.data = vec![0; abi::MAX_EVENT_BYTES + 1];
    assert_eq!(validate_input(&editor, &event), Err(HostError::LimitExceeded));
    event.data = vec![255];
    assert_eq!(validate_input(&editor, &event), Err(HostError::InvalidArgument));
    event.data = playbit_api::abi::text::TextInput::encode("", &[]).expect("empty text");
    assert_eq!(validate_input(&editor, &event), Ok(()));
    if let ViewNode::TextField { read_only, .. } = &mut editor {
        *read_only = true;
    }
    assert_eq!(validate_input(&editor, &event), Err(HostError::InvalidArgument));
    event.kind = EventKind::Submit;
    assert_eq!(validate_input(&editor, &event), Err(HostError::InvalidArgument));
    event.kind = EventKind::TextChanged;
    if let ViewNode::TextField { disabled, read_only, .. } = &mut editor {
        *disabled = true;
        *read_only = false;
    }
    assert_eq!(validate_input(&editor, &event), Err(HostError::InvalidArgument));
    let button = ViewNode::Button {
        id: 2,
        label: "Add".into(),
        style: playbit_api::ui::ButtonStyle::Default,
        disabled: false,
    };
    assert_eq!(validate_input(&button, &event), Err(HostError::InvalidArgument));
    let mut autocomplete = ViewNode::AutocompleteField {
        id: 2,
        value: String::new(),
        placeholder: String::new(),
        suggestions: vec![],
        disabled: false,
    };
    assert_eq!(validate_input(&autocomplete, &event), Ok(()));
    if let ViewNode::AutocompleteField { disabled, .. } = &mut autocomplete {
        *disabled = true;
    }
    assert_eq!(validate_input(&autocomplete, &event), Err(HostError::InvalidArgument));
}

#[test]
fn mutation_admits_memory_before_host_effects() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let handle = guest.volume_handle(ROOT).unwrap();
        let (_entry, _stale_entry) = super::test_support::entry_pointers(&mut guest, handle);
        stage_changes(&mut guest, &[]);
        let before = read(&mut guest, 0, 131072);
        assert_eq!(
            call(&mut guest, "create_begin", (_entry, u32::MAX, 0u32, 0u32, 65536u32)),
            HostError::InvalidArgument as i32
        );
        assert_eq!(read(&mut guest, 0, 131072), before);
        assert_eq!(records.lock().unwrap().creates, 0);
        assert_eq!(call(&mut guest, "create_begin", (_entry, 16u32, 0u32, 0u32, 65536u32)), 0);
        let pending = super::test_support::operation_handle(&mut guest, 65536);
        assert_eq!(
            scalar(&mut guest, Opcode::ObjectCreatePoll, [packed(pending), 65552, 0, 0, 0, 0]),
            -6
        );
        assert_eq!(records.lock().unwrap().creates, 0);
        execute_guest(&mut guest, &records, pending);
        assert_eq!(
            scalar(&mut guest, Opcode::ObjectCreatePoll, [packed(pending), 65552, 0, 0, 0, 0]),
            0
        );
        assert_eq!(read(&mut guest, 65552, 16), OBJECT);
        assert_eq!(records.lock().unwrap().creates, 1);
        assert_eq!(
            call(&mut guest, "log", (8u32, (abi::MAX_CALL_BYTES + 1) as u32)),
            HostError::InvalidArgument as i32
        );
    }
}

#[test]
fn event_retries_preserve_queue_and_guest_epochs_are_independent_of_handles() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut first, _) = guest(backend);
        let old_generation = first.generation();
        let old_handle = first.volume_handle(ROOT).unwrap();
        words(&mut first, 64, &[24, 0]);
        assert_eq!(call(&mut first, "event", (64u32, 0u32, 0u32)), 0);
        assert_eq!(read(&mut first, 72, 8), old_generation.to_le_bytes());
        assert_eq!(get_u32(&mut first, 80), EventKind::Started as u32);
        first
            .queue_event(
                old_generation,
                Event { kind: EventKind::Submit, target: 7, data: b"hello".to_vec() },
            )
            .unwrap();
        write(&mut first, 64, &[0xa5; 24]);
        words(&mut first, 64, &[24, 0]);
        write(&mut first, 128, &[0xa5; 5]);
        assert_eq!(call(&mut first, "event", (64u32, 128u32, 1u32)), 5);
        assert_eq!(get_u32(&mut first, 64), 24);
        assert_eq!(get_u32(&mut first, 68), 0);
        assert_eq!(read(&mut first, 72, 16), [0xa5; 16]);
        assert_eq!(read(&mut first, 128, 5), [0xa5; 5]);
        assert_eq!(first.store.data().app.events.len(), 1);
        assert_eq!(call(&mut first, "event", (64u32, 128u32, 5u32)), 5);
        assert_eq!(get_u32(&mut first, 80), EventKind::Submit as u32);
        assert_eq!(get_u32(&mut first, 84), 7);
        assert_eq!(read(&mut first, 128, 5), b"hello");
        assert_eq!(call(&mut first, "event", (64u32, 128u32, 5u32)), HostError::WouldBlock as i32);
        let (mut replacement, replacement_records) = guest(backend);
        assert_ne!(replacement.generation(), old_generation);
        assert_eq!(
            replacement.queue_event(
                old_generation,
                Event { kind: EventKind::Click, target: 1, data: vec![] }
            ),
            Err(HostError::StaleHandle)
        );
        // Capability numbers belong to their receiving guest's local namespace.
        assert_eq!(replacement.volume_handle(ROOT), Some(old_handle));
        assert_eq!(read_object(&mut replacement, old_handle, &replacement_records), 0);
        assert_eq!(replacement.store.data().app.events.len(), 1);
    }
}

#[test]
fn required_capabilities_roots_and_replaced_imports_fail_explicitly() {
    let runtime = Runtime::new().unwrap();
    let wasm = wat::parse_str(CALLER).unwrap();
    let mut missing = context();
    missing.required_capabilities.insert("file.write".to_owned());
    assert!(
        runtime
            .create_guest(
                &wasm,
                missing,
                Box::new(TestHost(Arc::new(Mutex::new(Records::default()))))
            )
            .is_err()
    );
    let mut roots = context();
    roots.volumes[0].is_root = true;
    assert!(
        runtime
            .create_guest(
                &wasm,
                roots,
                Box::new(TestHost(Arc::new(Mutex::new(Records::default()))))
            )
            .is_err()
    );
    assert!(
        runtime
            .validate_guest(
                &wat::parse_str(
                    "(module (memory (export \"memory\") 1) (func (export \"pb_main\")))"
                )
                .unwrap()
            )
            .is_ok()
    );
    for module in [
        "(module (import \"playbit\" \"log\" (func (param i32 i32) (result i32))) (memory (export \"memory\") 1) (func (export \"pb_main\")))",
        "(module (import \"playbit\" \"syscall\" (func)) (memory (export \"memory\") 1) (func (export \"pb_main\")))",
        "(module (import \"wasi_snapshot_preview1\" \"proc_exit\" (func (param i32))) (memory (export \"memory\") 1) (func (export \"pb_main\")))",
    ] {
        assert!(runtime.validate_guest(&wat::parse_str(module).unwrap()).is_err(), "{module}");
    }
}

#[test]
fn volume_ceiling_and_current_host_authority_both_apply() {
    let (mut guest, records) = guest(ExecutionBackend::Native);
    let read_only = guest.volume_handle([8; 16]).unwrap();
    let (_entry, _) = super::test_support::entry_pointers(&mut guest, read_only);
    stage_changes(&mut guest, &[]);
    assert_eq!(
        call(&mut guest, "create_begin", (_entry, 16u32, 0u32, 0u32, 65536u32)),
        HostError::AccessDenied as i32
    );
    assert_eq!(records.lock().unwrap().creates, 0);
    records.lock().unwrap().fields = vec![
        FieldChange { field_id: FIELD, values: Some(vec![Value::Integer(42)]) },
        FieldChange {
            field_id: [4; 16],
            values: Some(vec![Value::File { volume_id: [6; 16], entry_id: [7; 15] }]),
        },
    ];
    let handle = guest.volume_handle(ROOT).unwrap();
    assert_eq!(read_object(&mut guest, handle, &records), 0);
    assert_eq!(read(&mut guest, 72, 16), TYPE);
    assert_eq!(get_u32(&mut guest, 96), 1);
    assert_eq!(read(&mut guest, 1032, 16), FIELD);
    assert_eq!(get_u32(&mut guest, 8200), playbit_api::abi::data::INTEGER);
    assert_eq!(read(&mut guest, 8208, 8), 42i64.to_le_bytes());
    records.lock().unwrap().denied = true;
    assert_eq!(read_object(&mut guest, handle, &records), HostError::AccessDenied as i32);
}

#[test]
fn volume_changes_preserve_live_handles_and_invalidate_revoked_handles() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, _) = guest(backend);
        let generation = guest.generation();
        let root = guest.volume_handle(ROOT).unwrap();
        let (_entry, _stale_entry) = super::test_support::entry_pointers(&mut guest, root);
        let initial = guest.volume_handle([8; 16]).unwrap();
        let mut volumes = context().volumes;
        for volume in &mut volumes {
            volume.role = if volume.is_root { 1 } else { 3 };
        }
        volumes.reverse();
        assert!(guest.replace_volumes(volumes.clone()).unwrap());
        assert_eq!(guest.volume_handle(ROOT), Some(root));
        assert_eq!(guest.volume_handle([8; 16]), Some(initial));
        assert_eq!(resolve_volume(&guest.store.data().app, root, VolumeRights::READ), Ok(ROOT));
        volumes[1].rights = VolumeRights::ALL;
        guest.replace_volumes(volumes.clone()).unwrap();
        assert_eq!(
            resolve_volume(&guest.store.data().app, initial, VolumeRights::WRITE),
            Ok([8; 16])
        );
        volumes[1].rights = VolumeRights::READ;
        guest.replace_volumes(volumes.clone()).unwrap();
        assert_eq!(
            resolve_volume(&guest.store.data().app, initial, VolumeRights::WRITE),
            Err(HostError::AccessDenied)
        );
        for _ in 0..1000 {
            let previous = guest.volume_handle([8; 16]).unwrap();
            guest.replace_volumes(vec![volumes[0].clone()]).unwrap();
            assert_eq!(
                resolve_volume(&guest.store.data().app, previous, VolumeRights::READ),
                Err(HostError::StaleHandle)
            );
            guest.replace_volumes(volumes.clone()).unwrap();
            assert_ne!(guest.volume_handle([8; 16]), Some(previous));
            assert_eq!(
                resolve_volume(&guest.store.data().app, previous, VolumeRights::READ),
                Err(HostError::StaleHandle)
            );
            assert_eq!(guest.volume_handle(ROOT), Some(root));
        }
        assert_eq!(guest.generation(), generation);
        words(&mut guest, 64, &[64, 0, 0, 0, 1024, 2, 0, 80, 2048, 128, 0, 16, 4096, 4096, 0, 0]);
        stage_records(&mut guest, 1024, 2, 80);
        stage_records(&mut guest, 2048, 128, 16);
        assert_eq!(call(&mut guest, "volumes", 64u32), 0);
        assert_eq!(get_u32(&mut guest, 88), 2);
        for (index, volume) in volumes.iter().enumerate() {
            let offset = 1024 + index * 80;
            assert_eq!(get_u32(&mut guest, offset + 4), 80);
            let handle = guest.volume_handle(volume.id).unwrap();
            let (_entry, _stale_entry) = super::test_support::entry_pointers(&mut guest, handle);
            assert_eq!(read(&mut guest, offset + 8, 4), handle.slot.to_le_bytes());
            assert_eq!(get_u32(&mut guest, offset + 12), handle.generation);
            assert_eq!(read(&mut guest, offset + 16, 16), volume.id);
            assert_eq!(get_u32(&mut guest, offset + 48), volume.rights.0);
            assert_eq!(
                get_u32(&mut guest, offset + 52),
                u32::from(volume.is_root) | playbit_api::abi::data::VOLUME_PLAYBIT
            );
            let start = get_u32(&mut guest, offset + 32) as usize;
            let length = get_u32(&mut guest, offset + 36) as usize;
            assert_eq!(read(&mut guest, 4096 + start, length), volume.name.as_bytes());
            assert_eq!(get_u32(&mut guest, offset + 44) as usize, volume.tags.len());
            let start = get_u32(&mut guest, offset + 40) as usize;
            for (index, tag) in volume.tags.iter().enumerate() {
                let record = 2048 + (start + index) * 16;
                let start = get_u32(&mut guest, record + 8) as usize;
                let length = get_u32(&mut guest, record + 12) as usize;
                assert_eq!(read(&mut guest, 4096 + start, length), tag.as_bytes());
            }
        }
    }
}

#[test]
fn invalid_volume_change_is_atomic_and_removed_slots_reuse_new_generations() {
    let (mut guest, _) = guest(ExecutionBackend::Native);
    let original = context().volumes;
    let root = guest.volume_handle(ROOT).unwrap();
    let (_entry, _stale_entry) = super::test_support::entry_pointers(&mut guest, root);
    let personal = guest.volume_handle([8; 16]).unwrap();
    let mut changed_root = original.clone();
    changed_root[1].id = [99; 16];
    let mut invalid_name = original.clone();
    invalid_name[0].name.clear();
    for volumes in [vec![], changed_root, invalid_name, vec![original[1].clone(); 2]] {
        assert!(guest.replace_volumes(volumes).is_err());
        assert_eq!(guest.store.data().app.context.volumes, original);
        assert_eq!(guest.volume_handle(ROOT), Some(root));
        assert_eq!(guest.volume_handle([8; 16]), Some(personal));
    }
    let mut extra = original[0].clone();
    extra.id = [90; 16];
    let mut changed = original.clone();
    changed.push(extra);
    guest.replace_volumes(changed.clone()).unwrap();
    let removed = guest.volume_handle([90; 16]).unwrap();
    guest.replace_volumes(original.clone()).unwrap();
    guest.replace_volumes(changed).unwrap();
    let replacement = guest.volume_handle([90; 16]).unwrap();
    assert_eq!(removed.slot, replacement.slot);
    assert_ne!(removed.generation, replacement.generation);
    assert_eq!(
        super::handles::resolve_volume(&guest.store.data().app, removed, VolumeRights::READ),
        Err(HostError::StaleHandle)
    );
}

#[test]
#[ignore = "opt-in volume-context reconciliation benchmark"]
fn volume_context_change_measurement() {
    let (mut guest, _) = guest(ExecutionBackend::Native);
    let mut base = context().volumes;
    base[0].tags = vec!["session".into()];
    for count in [0, 4, 128, 254] {
        let mut selected = base.clone();
        for n in 0..count {
            selected.push(AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: (100 + n as u128).to_le_bytes(),
                name: "x".repeat(64),
                tags: vec!["personal".into()],
                rights: VolumeRights::READ,
                is_root: false,
                role: 0,
            });
        }
        let mut samples = Vec::with_capacity(2000);
        for _ in 0..1000 {
            for volumes in [selected.clone(), base.clone()] {
                let begin = Instant::now();
                guest.replace_volumes(volumes).unwrap();
                samples.push(begin.elapsed().as_nanos());
            }
        }
        samples.sort_unstable();
        crate::diagnostic!(
            "volume context personal={count} cycles=1000 n=2000 p50={} p99={} max={} ns",
            samples[999],
            samples[1979],
            samples[1999]
        );
    }
}

#[test]
fn malformed_edits_never_reach_storage_and_values_round_trip() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let handle = guest.volume_handle(ROOT).unwrap();
        let (_entry, _stale_entry) = super::test_support::entry_pointers(&mut guest, handle);
        let changes = vec![FieldChange {
            field_id: FIELD,
            values: Some(vec![
                Value::Text("text".into()),
                Value::Boolean(true),
                Value::Integer(-5),
                Value::Float(1.25),
                Value::Bytes(vec![0, 255]),
                Value::Reference { volume_id: ROOT, database_id: [5; 15], object_id: OBJECT },
            ]),
        }];
        stage_changes(&mut guest, &changes);
        for (pointer, count) in [(0u32, 1u32), (1025, 1), (131056, 1), (1024, abi::MAX_FIELDS + 1)]
        {
            assert!(call(&mut guest, "edit_begin", (_entry, 32u32, pointer, count, 65536u32)) < 0);
        }
        // Invalid presence, value pointer, value count, tag, and UTF-8 payload.
        for (offset, value) in [
            (1060, 2u32),
            (1048, 8193),
            (1052, abi::MAX_VALUES + 1),
            (8200, 99),
            (32768, 0xffffffff),
        ] {
            stage_changes(&mut guest, &changes);
            put_u32(&mut guest, offset, value);
            assert!(call(&mut guest, "edit_begin", (_entry, 32u32, 1024u32, 1u32, 65536u32)) < 0);
        }
        assert_eq!(records.lock().unwrap().edits, 0);
        stage_changes(&mut guest, &changes);
        assert_eq!(call(&mut guest, "edit_begin", (_entry, 32u32, 1024u32, 1u32, 65536u32)), 0);
        let pending = super::test_support::operation_handle(&mut guest, 65536);
        assert_eq!(records.lock().unwrap().edits, 0);
        assert_eq!(
            scalar(&mut guest, Opcode::ObjectEditTake, [packed(pending), 0, 0, 0, 0, 0]),
            -6
        );
        execute_guest(&mut guest, &records, pending);
        assert_eq!(scalar(&mut guest, Opcode::ObjectEditTake, [packed(pending), 0, 0, 0, 0, 0]), 0);
        assert_eq!(records.lock().unwrap().fields, changes);
        assert_eq!(records.lock().unwrap().edits, 1);
    }
}

#[test]
fn file_references_round_trip_and_cannot_forge_content_metadata() {
    let changes = vec![FieldChange {
        field_id: FIELD,
        values: Some(vec![Value::File { volume_id: [6; 16], entry_id: [7; 15] }]),
    }];
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let handle = guest.volume_handle(ROOT).unwrap();
        let (_entry, _stale_entry) = super::test_support::entry_pointers(&mut guest, handle);
        stage_changes(&mut guest, &changes);
        write(&mut guest, 8208, &1_u64.to_le_bytes());
        assert_eq!(
            call(&mut guest, "edit_begin", (_entry, 32u32, 1024u32, 1u32, 65536u32)),
            HostError::InvalidArgument as i32
        );
        assert_eq!(records.lock().unwrap().edits, 0);
        stage_changes(&mut guest, &changes);
        assert_eq!(call(&mut guest, "edit_begin", (_entry, 32u32, 1024u32, 1u32, 65536u32)), 0);
        let pending = super::test_support::operation_handle(&mut guest, 65536);
        execute_guest(&mut guest, &records, pending);
        assert_eq!(scalar(&mut guest, Opcode::ObjectEditTake, [packed(pending), 0, 0, 0, 0, 0]), 0);
        assert_eq!(records.lock().unwrap().edits, 1);
        records.lock().unwrap().fields = changes.clone();
        assert_eq!(read_object(&mut guest, handle, &records), 0);
        assert_eq!(get_u32(&mut guest, 96), 1);
        assert_eq!(get_u32(&mut guest, 112), 1);
        assert_eq!(get_u32(&mut guest, 128), 0);
        assert_eq!(read(&mut guest, 1032, 16), FIELD);
        assert_eq!(get_u32(&mut guest, 8200), playbit_api::abi::data::FILE);
        assert_eq!(read(&mut guest, 8208, 8), [0; 8]);
        assert_eq!(read(&mut guest, 8216, 16), [6; 16]);
        assert_eq!(read(&mut guest, 8248, 15), [7; 15]);
    }
}

#[test]
fn trapped_generation_refuses_reentry_input_and_wakes() {
    let wasm = CALLER.replace(
        "(func (export \"pb_main\"))",
        "(func (export \"pb_main\") (i32.store (i32.const 0) (i32.add (i32.load \
         (i32.const 0)) (i32.const 1))) unreachable)",
    );
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let runtime = Runtime::with_backend(backend).unwrap();
        let mut guest = runtime
            .create_guest(
                &wat::parse_str(&wasm).unwrap(),
                context(),
                Box::new(TestHost(Arc::new(Mutex::new(Records::default())))),
            )
            .unwrap();
        let generation = guest.generation();
        guest.store.data_mut().app.wake_deadline = Some(Instant::now());
        assert!(matches!(guest.pump(), Err(RuntimeError::GuestTrap(_))));
        let memory = guest.instance.get_memory(&mut guest.store, "memory").unwrap();
        let after = memory.data(&guest.store)[..4].to_vec();
        for _ in 0..3 {
            assert!(matches!(guest.pump(), Err(RuntimeError::GuestHalted)));
            assert_eq!(memory.data(&guest.store)[..4], after);
            assert_eq!(
                guest.queue_event(
                    generation,
                    Event { kind: EventKind::Click, target: 1, data: vec![] }
                ),
                Err(HostError::Conflict)
            );
            assert!(!guest.has_ready_work(Instant::now()));
            assert!(!guest.wake_due(Instant::now()));
            assert_eq!(guest.wake_after(Instant::now()), None);
        }
    }
}

#[test]
fn busy_guests_and_unbounded_event_queues_fail_safely() {
    let infinite =
        CALLER.replace("(func (export \"pb_main\"))", "(func (export \"pb_main\") (loop br 0))");
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let runtime =
            Runtime::with_backend_and_fuel_limit(backend, std::num::NonZeroU64::new(1000)).unwrap();
        let mut guest = runtime
            .create_guest(
                &wat::parse_str(&infinite).unwrap(),
                context(),
                Box::new(TestHost(Arc::new(Mutex::new(Records::default())))),
            )
            .unwrap();
        assert!(matches!(guest.pump(), Err(RuntimeError::FuelExhausted)));
        assert!(matches!(guest.pump(), Err(RuntimeError::GuestHalted)));
    }
    let (mut guest, _) = guest(ExecutionBackend::Native);
    let generation = guest.generation();
    for _ in 1..MAX_QUEUED_EVENTS {
        guest
            .queue_event(generation, Event { kind: EventKind::Click, target: 1, data: vec![] })
            .unwrap();
    }
    assert_eq!(
        guest.queue_event(generation, Event { kind: EventKind::Click, target: 1, data: vec![] }),
        Err(HostError::LimitExceeded)
    );
}

#[test]
fn update_deferral_is_bounded_and_ordered() {
    // Deferrals bind to the current update and cannot survive a newer selection.
    let (mut guest, _) = guest(ExecutionBackend::Native);
    let generation = guest.generation();
    guest.notify_update(generation, PendingUpdate { generation: 2, build_id: [1; 32] }).unwrap();
    assert_eq!(call(&mut guest, "defer", 1u64), HostError::Conflict as i32);
    assert_eq!(call(&mut guest, "defer", 2u64), 0);
    assert_eq!(guest.deferred_update(), Some(2));
    guest.notify_update(generation, PendingUpdate { generation: 3, build_id: [2; 32] }).unwrap();
    assert_eq!(guest.deferred_update(), None);
    assert_eq!(
        guest.notify_update(generation, PendingUpdate { generation: 2, build_id: [1; 32] }),
        Err(HostError::Conflict)
    );
    assert_eq!(call(&mut guest, "defer", 2u64), HostError::Conflict as i32);
}
