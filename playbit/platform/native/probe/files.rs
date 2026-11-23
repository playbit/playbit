//! Independent native C/Rust File consumers and durable publication journey.
use super::*;
use playbit::runtime::{
    host::files::{
        Action, Call, Output, Root,
        capacity::{Capacity, Kind, Retention},
    },
    syscall::HostResult,
};
use playbit_api::{
    abi::{data as data_abi, files as ffi, syscall::Error},
    context::{Completion, Context, FileHandle, OperationHandle, PollError},
};
use std::sync::{Arc, Mutex};
unsafe extern "C" {
    fn probe_file_calls(context: *mut abi::Context, root: FileHandle) -> i32;
    fn probe_file_layout(index: u32) -> u64;
    fn probe_persistent_file_begin(
        context: *mut abi::Context,
        file: FileHandle,
        bytes: *const u8,
        length: u32,
        output: *mut OperationHandle,
    ) -> i32;
    fn probe_persistent_file_take(context: *mut abi::Context, handle: OperationHandle) -> i32;
}
enum Opened {
    Reader(Vec<u8>),
    Writer(Vec<u8>),
}
#[derive(Default)]
struct Content {
    bytes: Vec<u8>,
    capacity: Arc<Capacity>,
    root: Option<Root>,
    files: BTreeMap<u64, Opened>,
    failure: Option<HostError>,
    malformed: bool,
    next: u64,
    completions: BTreeMap<u64, (HostResult<Arc<Output>>, Retention)>,
}
impl Content {
    fn execute(&mut self, call: &Call) -> HostResult<Output> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        match &call.action {
            Action::Open { path, mode } => {
                assert_eq!(call.file, 1);
                assert_eq!(path, b"probe.txt");
                self.next += 1;
                let opened = if *mode == ffi::FILE_READ {
                    Opened::Reader(self.bytes.clone())
                } else {
                    Opened::Writer(Vec::new())
                };
                self.files.insert(self.next, opened);
                Ok(Output::File { handle: self.next })
            }
            Action::Read { offset, length } => {
                let Opened::Reader(bytes) = &self.files[&call.file] else { panic!("reader") };
                let start = (*offset as usize).min(bytes.len());
                let end = (start + *length as usize).min(bytes.len());
                Ok(Output::Read {
                    bytes: if self.malformed {
                        vec![0; *length as usize + 1]
                    } else {
                        bytes[start..end].to_vec()
                    },
                })
            }
            Action::Write { offset, bytes } => {
                let Opened::Writer(staged) = self.files.get_mut(&call.file).unwrap() else {
                    panic!("writer")
                };
                if !bytes.is_empty() {
                    staged.resize(staged.len().max(*offset as usize + bytes.len()), 0);
                    staged[*offset as usize..*offset as usize + bytes.len()].copy_from_slice(bytes);
                }
                Ok(Output::Written)
            }
            Action::Status => {
                let (Opened::Reader(bytes) | Opened::Writer(bytes)) = &self.files[&call.file];
                Ok(ffi::FileStatus::private(bytes.len() as u64, true).into())
            }
            Action::Close { flags } => {
                let opened = self.files.remove(&call.file).unwrap();
                let publication = if let Opened::Writer(bytes) = opened {
                    if *flags != ffi::FILE_CLOSE_DISCARD {
                        self.bytes = bytes;
                        ffi::FILE_PUBLISHED
                    } else {
                        ffi::FILE_NOT_PUBLISHED
                    }
                } else {
                    ffi::FILE_NOT_PUBLISHED
                };
                Ok(Output::Close { error: 0, publication, recovery: None })
            }
            _ => Err(HostError::Unsupported),
        }
    }
}
struct FileHost(Arc<Mutex<Content>>);
impl Host for FileHost {
    fn file_capacity(&self) -> Arc<Capacity> {
        self.0.lock().unwrap().capacity.clone()
    }
    fn file_roots(&mut self, volumes: &[AvailableVolume]) -> HostResult<Vec<Root>> {
        let mut state = self.0.lock().unwrap();
        if state.root.is_none() {
            let credit = state.capacity.one(Kind::File)?;
            state.next = 1;
            state.root = Some(Root {
                replaces: None,
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                volume: volumes[0].id,
                grant: [54; 16],
                engine: 1,
                writable: volumes[0].rights.contains(VolumeRights::WRITE),
                credit,
            });
        }
        Ok(vec![state.root.as_ref().unwrap().clone()])
    }
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::FileOpenBegin,
            Opcode::FileOpenTake,
            Opcode::FileReadBegin,
            Opcode::FileReadTake,
            Opcode::FileWriteBegin,
            Opcode::FileWriteTake,
            Opcode::FileCloseBegin,
            Opcode::FileCloseTake,
            Opcode::FileStatusBegin,
            Opcode::FileStatusTake,
            Opcode::OperationDispose,
        ]
    }
    fn begin_file(&mut self, _: u64, call: Arc<Call>, retention: Retention) -> HostResult<u64> {
        // Controlled memory only. Production disk I/O is qualified by the durable journey below.
        let mut state = self.0.lock().unwrap();
        let result = state.execute(&call).map(Arc::new);
        state.next += 1;
        let token = state.next;
        state.completions.insert(token, (result, retention));
        Ok(token)
    }
    fn peek_file(&mut self, token: u64) -> HostResult<Arc<Output>> {
        self.0.lock().unwrap().completions[&token].0.clone()
    }
    fn release_file(&mut self, token: u64, claimed: bool) {
        let mut state = self.0.lock().unwrap();
        if let Some((Ok(output), _)) = state.completions.remove(&token) {
            if !claimed && let Output::File { handle } = &*output {
                state.files.remove(handle);
            }
        }
    }
    fn discard_file(&mut self, file: u64, _: bool, _: Retention) -> HostResult<()> {
        self.0.lock().unwrap().files.remove(&file);
        Ok(())
    }
}
fn root(raw: *mut abi::Context) -> FileHandle {
    let mut rows = [data_abi::Volume::default()];
    let mut text = [0; 16];
    let mut output = data_abi::Volumes {
        revision: 0,
        volumes_data: rows.as_mut_ptr(),
        volumes_capacity: 1,
        volumes_stride: size_of::<data_abi::Volume>() as u32,
        bytes_data: text.as_mut_ptr(),
        bytes_capacity: 16,
        size: size_of::<data_abi::Volumes>() as u32,
        written: 0,
        volumes_count: 0,
        tags_data: std::ptr::null_mut(),
        tags_capacity: 0,
        tags_count: 0,
        tags_stride: size_of::<data_abi::Range>() as u32,
        bytes_count: 0,
    };
    assert_eq!(unsafe { data_abi::pb_volumes(raw, &mut output) }, 0);
    FileHandle(rows[0].file)
}
unsafe extern "C" fn init(
    _: *mut abi::Context,
    user: *mut c_void,
    output: *mut *mut c_void,
) -> i32 {
    unsafe {
        output.write(user);
    }
    0
}
unsafe extern "C" fn destroy(_: *mut c_void) {}
fn callbacks(turn: abi::Turn, user: *mut c_void) -> abi::App {
    abi::App {
        init: Some(init),
        turn: Some(turn),
        destroy: Some(destroy),
        user_data: user,
        size: size_of::<abi::App>() as u32,
        written: 0,
    }
}
fn opened(context: &mut Context<'_>, root: FileHandle, mode: u32) -> FileHandle {
    let operation = context.begin_open_file(root, b"probe.txt", mode).unwrap();
    let Completion::Ready(file) = context.take_open_file(operation).unwrap() else {
        panic!("opened")
    };
    file
}
fn closed(context: &mut Context<'_>, file: FileHandle, flags: u32) {
    let operation = context.begin_close_file(file, flags).unwrap();
    let Completion::Ready(receipt) = context.take_close_file(operation).unwrap() else {
        panic!("closed")
    };
    assert_eq!(receipt.error, 0);
}
unsafe extern "C" fn normal_turn(raw: *mut abi::Context, _: *mut c_void) -> i32 {
    let root = root(raw);
    assert_eq!(unsafe { probe_file_calls(raw, root) }, 0);
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    let writer = opened(&mut context, root, ffi::FILE_WRITE);
    let write = context.begin_write_file(writer, 0, b"hello").unwrap();
    assert_eq!(context.take_write_file(write), Ok(Completion::Ready(())));
    closed(&mut context, writer, 0);
    let reader = opened(&mut context, root, ffi::FILE_READ);
    let read = context.begin_read_file(reader, 0, 5).unwrap();
    assert_eq!(context.take_read_file(read, &mut []), Err(PollError::BufferTooSmall(5)));
    let mut bytes = [0; 5];
    assert_eq!(context.take_read_file(read, &mut bytes), Ok(Completion::Ready(&b"hello"[..])));
    closed(&mut context, reader, 0);
    assert_eq!(unsafe { ffi::pb_file_dispose(raw, root) }, 0);
    0
}
#[test]
fn c_file_layouts_and_operations_match_rust() {
    let layout = [
        size_of::<ffi::FileHandle>(),
        align_of::<ffi::FileHandle>(),
        size_of::<u32>(),
        size_of::<ffi::FileStatus>(),
        align_of::<ffi::FileStatus>(),
        offset_of!(ffi::FileStatus, modification_time),
        size_of::<ffi::FileCloseResult>(),
        align_of::<ffi::FileCloseResult>(),
        offset_of!(ffi::FileCloseResult, recovery),
        size_of::<ffi::FileListEntry>(),
        align_of::<ffi::FileListEntry>(),
        offset_of!(ffi::FileListEntry, name_data),
    ];
    for (index, expected) in layout.into_iter().enumerate() {
        assert_eq!(unsafe { probe_file_layout(index as u32) }, expected as u64);
    }
    let content = Arc::new(Mutex::new(Content::default()));
    let mut instance = unsafe {
        NativeInstance::new(
            context(50),
            Box::new(FileHost(content.clone())),
            callbacks(normal_turn, std::ptr::null_mut()),
        )
        .unwrap()
    };
    instance.pump().unwrap();
    let state = content.lock().unwrap();
    assert_eq!(state.bytes, b"hello");
    assert!(state.files.is_empty() && state.completions.is_empty());
}

// Use the SDK's registered callback and automatic FileResources owner.
#[unsafe(no_mangle)]
fn playbit_app() -> playbit_api::App {
    playbit_api::app(
        (),
        |_, context, _| {
            let mut rows = [data_abi::Volume::default()];
            let mut text = [0; 16];
            let root = context
                .available_volumes(&mut rows, &mut [], &mut text)
                .unwrap()
                .take_file(0)
                .unwrap();
            let open = root.open(context, b"probe.txt", ffi::FILE_READ).unwrap();
            let mut payload = [0; 8];
            let ready = context.poll_event_deadline(u64::MAX, &mut payload).unwrap();
            assert_eq!(ready.operation(), Ok(open.handle()));
            drop(open);
            drop(root);
            assert_eq!(
                context.poll_event_deadline(u64::MAX, &mut payload),
                Err(PollError::Host(Error::Closed))
            );
        },
        true,
    )
}

#[test]
fn registered_guest_releases_dropped_file_work_before_its_next_indefinite_poll() {
    let content = Arc::new(Mutex::new(Content::default()));
    let mut ctx = context(50);
    ctx.required_capabilities.insert("event.poll.deadline".into());
    // SAFETY: the linked SDK retains the registered callback and its state for the instance.
    let mut instance = unsafe {
        NativeInstance::new(ctx, Box::new(FileHost(content.clone())), playbit_api::native_app())
            .unwrap()
    };
    instance.pump().unwrap();
    let token = *content.lock().unwrap().completions.keys().next().unwrap();
    instance.notify_data(instance.generation(), token).unwrap();
    instance.pump().unwrap();
    instance.deactivate();
    assert!(!instance.has_ready_work(std::time::Instant::now()));
    assert!(!instance.drain_pending(), "a dropped File open must not keep Quit pending");
    let state = content.lock().unwrap();
    assert!(state.completions.is_empty() && state.files.is_empty());
}

struct Expected {
    read: Error,
    write: Error,
}
unsafe extern "C" fn errors_turn(raw: *mut abi::Context, user: *mut c_void) -> i32 {
    let expected = unsafe { &*user.cast::<Expected>() };
    let root = root(raw);
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    for (mode, expected) in [(ffi::FILE_READ, expected.read), (ffi::FILE_WRITE, expected.write)] {
        let actual = context
            .begin_open_file(root, b"probe.txt", mode)
            .and_then(|op| context.take_open_file(op));
        assert_eq!(actual, Err(expected));
    }
    assert_eq!(unsafe { ffi::pb_file_dispose(raw, root) }, 0);
    0
}
#[test]
fn unsupported_and_host_denials_preserve_data() {
    for (failure, supported, error) in [
        (None, false, Error::Unsupported),
        (Some(HostError::AccessDenied), true, Error::AccessDenied),
        (Some(HostError::Unavailable), true, Error::Unavailable),
    ] {
        let content = Arc::new(Mutex::new(Content {
            bytes: b"retained".to_vec(),
            failure,
            ..Default::default()
        }));
        let host: Box<dyn Host> =
            if supported { Box::new(FileHost(content.clone())) } else { Box::new(LocalHost) };
        let mut expected = Expected { read: error, write: error };
        let mut instance = unsafe {
            NativeInstance::new(
                context(50),
                host,
                callbacks(errors_turn, (&mut expected as *mut Expected).cast()),
            )
            .unwrap()
        };
        instance.pump().unwrap();
        assert_eq!(content.lock().unwrap().bytes, b"retained");
    }
}
unsafe extern "C" fn malformed_turn(raw: *mut abi::Context, _: *mut c_void) -> i32 {
    let root = root(raw);
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    let file = opened(&mut context, root, ffi::FILE_READ);
    let operation = context.begin_read_file(file, 0, 5).unwrap();
    let mut bytes = [0x55; 8];
    let mut count = 77;
    assert_eq!(
        unsafe { ffi::pb_file_read_take(raw, operation, bytes.as_mut_ptr(), 8, &mut count) },
        Error::Internal as i64
    );
    assert_eq!((count, bytes), (77, [0x55; 8]));
    assert_eq!(unsafe { ffi::pb_file_dispose(raw, file) }, 0);
    assert_eq!(unsafe { ffi::pb_file_dispose(raw, root) }, 0);
    0
}
#[test]
fn malformed_host_read_never_reaches_c_outputs() {
    let content = Arc::new(Mutex::new(Content { malformed: true, ..Default::default() }));
    let mut instance = unsafe {
        NativeInstance::new(
            context(50),
            Box::new(FileHost(content)),
            callbacks(malformed_turn, std::ptr::null_mut()),
        )
        .unwrap()
    };
    instance.pump().unwrap();
}
unsafe extern "C" fn limits_turn(raw: *mut abi::Context, _: *mut c_void) -> i32 {
    let root = root(raw);
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    let file = opened(&mut context, root, ffi::FILE_WRITE);
    let mut bytes = vec![42; ffi::MAX_FILE_WRITE as usize];
    let operation = context.begin_write_file(file, 0, &bytes).unwrap();
    assert_eq!(context.take_write_file(operation), Ok(Completion::Ready(())));
    bytes.push(0);
    assert_eq!(context.begin_write_file(file, 0, &bytes), Err(Error::LimitExceeded));
    closed(&mut context, file, 0);
    let file = opened(&mut context, root, ffi::FILE_READ);
    assert_eq!(context.begin_read_file(file, 0, 65537), Err(Error::LimitExceeded));
    assert_eq!(context.begin_read_file(file, u64::MAX, 1), Err(Error::LimitExceeded));
    let operation = context.begin_read_file(file, 1024 * 1024 - 65536, 65536).unwrap();
    let mut storage = vec![0; 65537];
    assert_eq!(
        context.take_read_file(operation, &mut storage),
        Ok(Completion::Ready(&[42; 65536][..]))
    );
    closed(&mut context, file, 0);
    assert_eq!(unsafe { ffi::pb_file_dispose(raw, root) }, 0);
    0
}
#[test]
fn maximum_native_file_write_and_read_are_bounded() {
    let content = Arc::new(Mutex::new(Content::default()));
    let mut instance = unsafe {
        NativeInstance::new(
            context(50),
            Box::new(FileHost(content.clone())),
            callbacks(limits_turn, std::ptr::null_mut()),
        )
        .unwrap()
    };
    instance.pump().unwrap();
    assert_eq!(content.lock().unwrap().bytes.len(), ffi::MAX_FILE_WRITE as usize);
}

struct PersistentProbe {
    phase: u8,
    file: Option<FileHandle>,
    content: Vec<u8>,
    rust: bool,
    handle: Option<OperationHandle>,
    result: Option<i32>,
}
unsafe extern "C" fn persistent_turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    let probe = unsafe { &mut *state.cast::<PersistentProbe>() };
    if probe.result.is_some() {
        return 0;
    }
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    if probe.handle.is_none() {
        let directory = root(raw);
        let operation = context.begin_open_file(directory, b"probe.txt", ffi::FILE_WRITE);
        assert_eq!(unsafe { ffi::pb_file_dispose(raw, directory) }, 0);
        match operation {
            Ok(operation) => {
                probe.handle = Some(operation);
                probe.phase = 0;
            }
            Err(error) => {
                probe.result = Some(error as i32);
                return 0;
            }
        }
    }
    for _ in 0..3 {
        let handle = probe.handle.unwrap();
        let next = match probe.phase {
            0 => match context.take_open_file(handle) {
                Ok(Completion::Pending) => return 0,
                Ok(Completion::Ready(file)) => {
                    probe.file = Some(file);
                    if probe.rust {
                        context.begin_write_file(file, 0, &probe.content)
                    } else {
                        let mut operation = OperationHandle::default();
                        let status = unsafe {
                            probe_persistent_file_begin(
                                raw,
                                file,
                                probe.content.as_ptr(),
                                probe.content.len() as u32,
                                &mut operation,
                            )
                        };
                        if status == 0 {
                            Ok(operation)
                        } else {
                            Err(Error::try_from(status).unwrap())
                        }
                    }
                }
                Err(error) => Err(error),
            },
            1 => {
                let result = if probe.rust {
                    context.take_write_file(handle)
                } else {
                    match unsafe { probe_persistent_file_take(raw, handle) } {
                        0 => Ok(Completion::Ready(())),
                        -6 => Ok(Completion::Pending),
                        error => Err(Error::try_from(error).unwrap()),
                    }
                };
                match result {
                    Ok(Completion::Pending) => return 0,
                    Ok(Completion::Ready(())) => {
                        context.begin_close_file(probe.file.unwrap(), ffi::FILE_CLOSE_SYNC)
                    }
                    Err(error) => Err(error),
                }
            }
            2 => {
                match context.take_close_file(handle) {
                    Ok(Completion::Pending) => return 0,
                    Ok(Completion::Ready(receipt)) => {
                        probe.result = Some(receipt.error);
                        if receipt.error == 0 {
                            assert_eq!(receipt.publication, ffi::FILE_PUBLISHED);
                        }
                    }
                    Err(error) => probe.result = Some(error as i32),
                }
                return 0;
            }
            _ => unreachable!(),
        };
        match next {
            Ok(operation) => {
                probe.handle = Some(operation);
                probe.phase += 1;
            }
            Err(error) => {
                probe.result = Some(error as i32);
                return 0;
            }
        }
    }
    0
}
fn persistent_write(
    services: &playbit::engine::Services,
    volume: playbit::store::Id,
    access: playbit::store::Access,
    rights: VolumeRights,
    probe: &mut PersistentProbe,
) -> i32 {
    let mut ctx = context(50);
    ctx.identity.account_id = *services.store.account().id.as_bytes();
    ctx.volumes[0].id = *volume.as_bytes();
    ctx.volumes[0].rights = rights;
    let backend = playbit::engine::data_host::DataHost {
        store: services.store.clone(),
        access: access.clone(),
        changed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let (mut host, inbox) =
        playbit::engine::in_process_host::InProcessHost::local(backend, Arc::new(|| {})).unwrap();
    ctx.volumes = match host.enable_files(services, access, None, None, &ctx.volumes) {
        Ok(volumes) => volumes,
        Err(error) => return error as i32,
    };
    probe.handle = None;
    probe.file = None;
    probe.phase = 0;
    probe.result = None;
    // SAFETY: this function retains the probe until the callback owner is destroyed.
    let mut instance = unsafe {
        NativeInstance::new(
            ctx,
            Box::new(host),
            callbacks(persistent_turn, (probe as *mut PersistentProbe).cast()),
        )
        .unwrap()
    };
    instance.pump().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while probe.result.is_none() && std::time::Instant::now() < deadline {
        for (generation, token, _) in inbox.ready() {
            instance.notify_data(generation, token).unwrap();
        }
        if instance.has_ready_work(std::time::Instant::now()) {
            instance.pump().unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    probe.result.expect("finite durable completion")
}
#[test]
fn c_and_rust_write_real_files_that_survive_reopen_and_authority_denials() {
    use playbit::{
        engine::Services,
        store::{Access, Id},
    };
    let directory = super::data_host::Directory::new();
    let services = Services::open(&directory.0, || {}).unwrap();
    let access = Access::owner(services.store.account().id);
    let volume = services.store.create_volume("Native files", false).unwrap().id;
    // Provision an independent named blob outside guest callbacks.
    let entry = services.store.create_blob(&access, volume, "probe.txt", b"").unwrap();
    let mut probe = PersistentProbe {
        phase: 0,
        file: None,
        content: vec![42; 4096],
        rust: false,
        handle: None,
        result: None,
    };
    assert_eq!(
        persistent_write(&services, volume, access.clone(), VolumeRights::ALL, &mut probe),
        0
    );
    assert_eq!(services.store.read_blob_entry(&access, volume, entry.id).unwrap(), probe.content);
    probe.rust = true;
    probe.content = vec![24; 4096];
    assert_eq!(
        persistent_write(&services, volume, access.clone(), VolumeRights::ALL, &mut probe),
        0
    );
    drop(services);
    let services = Services::open(&directory.0, || {}).unwrap();
    assert_eq!(services.store.read_blob_entry(&access, volume, entry.id).unwrap(), probe.content);
    for (access_override, rights, rust) in [
        (access.clone(), VolumeRights::READ, false),
        (Access::owner(Id::from_bytes([99; 16])), VolumeRights::ALL, true),
    ] {
        probe.rust = rust;
        probe.content = b"must not replace".to_vec();
        assert_eq!(
            persistent_write(&services, volume, access_override, rights, &mut probe),
            Error::AccessDenied as i32
        );
        assert_eq!(
            services.store.read_blob_entry(&access, volume, entry.id).unwrap(),
            vec![24; 4096]
        );
    }
}
