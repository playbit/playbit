//! Owns registered app state. Rust retains its type through dispatch and destruction.
#![allow(unsafe_code, reason = "The SDK owns the app allocation and target entry boundaries.")]

extern crate alloc;

#[cfg(not(target_arch = "wasm32"))]
use crate::abi::operations as abi;
use crate::context::{Context, ContextState};
use crate::ui::ViewEncoder;
use alloc::boxed::Box;
#[cfg(not(target_arch = "wasm32"))]
use core::ffi::c_void;

type TurnCallback = dyn FnMut(&mut Context<'_>, &mut ViewEncoder<'_>);

/// Owned app registration, linked with the SDK inside the guest binary.
pub struct App {
    turn: Box<TurnCallback>,
    files: crate::context::FileResources,
    quit_on_last_window: bool,
    context: ContextState,
}

/// Construct one instance. The SDK retains its state until the host destroys it.
/// Each turn receives a context and encoder sharing the preprocessed event snapshot.
/// `quit_on_last_window` declares whether closing its last window begins Quit.
/// False retains the execution owner; explicit Quit always drains accepted work.
pub fn app<T: 'static>(
    mut state: T,
    turn: fn(&mut T, &mut Context<'_>, &mut ViewEncoder<'_>),
    quit_on_last_window: bool,
) -> App {
    App {
        turn: Box::new(move |context, view| turn(&mut state, context, view)),
        quit_on_last_window,
        context: ContextState::default(),
        files: Default::default(),
    }
}

// Registration stays inside one Rust binary; only the host boundary uses C layouts.
unsafe extern "Rust" {
    fn playbit_app() -> App;
}

/// Rust hosts attach this descriptor once per instance inside their Rust binary.
#[cfg(not(target_arch = "wasm32"))]
pub fn native_app() -> abi::App {
    abi::App {
        size: core::mem::size_of::<abi::App>() as u32,
        written: 0,
        init: Some(init),
        turn: Some(turn),
        destroy: Some(destroy),
        user_data: core::ptr::null_mut(),
    }
}

/// Publish the native callbacks into caller-owned, size-prefixed storage.
/// # Safety
/// `output` lends its declared writable allocation. Callback code remains valid
/// while the linked guest library remains loaded; this does not create app state.
#[cfg(all(not(target_arch = "wasm32"), any(test, not(feature = "host"))))]
#[cfg_attr(not(feature = "host"), unsafe(export_name = "pb_app"))]
pub unsafe extern "C" fn native_app_export(output: *mut abi::App) -> i32 {
    // SAFETY: the caller lends the complete declared output; there are no inputs
    // or effects before admission, and no callback is invoked here.
    unsafe {
        crate::abi::record::clear_native(output)
            .and_then(|()| crate::abi::record::write_native(output, native_app()))
            .map_or_else(|error| error as i32, |()| 0)
    }
}

#[cfg(not(target_arch = "wasm32"))]
unsafe extern "C" fn init(raw: *mut abi::Context, _: *mut c_void, output: *mut *mut c_void) -> i32 {
    use crate::abi::syscall::Error;
    if output.is_null() {
        return Error::InvalidArgument as i32;
    }
    // SAFETY: the host supplies exclusive output, including on failed initialization.
    unsafe { output.write(core::ptr::null_mut()) };
    if raw.is_null() {
        return Error::InvalidArgument as i32;
    }
    // SAFETY: the linked application supplies the registration symbol.
    let app = unsafe { playbit_app() };
    let state = Box::into_raw(Box::new(app)) as *mut c_void;
    // SAFETY: ownership passes to the host until its single destroy callback.
    unsafe { output.write(state) };
    0
}

#[cfg(not(target_arch = "wasm32"))]
unsafe extern "C" fn turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    use crate::abi::syscall::Error;
    if state.is_null() {
        return Error::InvalidArgument as i32;
    }
    // SAFETY: successful init supplied this state; the host serializes callbacks.
    let app = unsafe { &mut *(state as *mut App) };
    // SAFETY: the native host lends its live context for this callback only.
    let (mut context, mut view) = match unsafe { Context::from_raw(raw, &mut app.context) } {
        Ok(turn) => turn,
        Err(error) => return error as i32,
    };
    if let Err(error) = context.quit_on_last_window(app.quit_on_last_window) {
        return error as i32;
    }
    if let Err(error) = context.attach_files(&app.files) {
        return error as i32;
    }
    (app.turn)(&mut context, &mut view);
    let _ = context.trace_flush();
    let events = context.event_status();
    let files = context.flush_files();
    match events.and(files) {
        Ok(()) => 0,
        Err(error) => error as i32,
    }
}

#[cfg(not(target_arch = "wasm32"))]
unsafe extern "C" fn destroy(state: *mut c_void) {
    if !state.is_null() {
        // SAFETY: successful init transferred this Box; the host destroys it once.
        unsafe { drop(Box::from_raw(state as *mut App)) };
    }
}

#[cfg(all(target_arch = "wasm32", feature = "guest"))]
mod wasm {
    use super::*;
    use core::{
        cell::UnsafeCell,
        sync::atomic::{AtomicBool, Ordering},
    };

    struct Instance {
        active: AtomicBool,
        app: UnsafeCell<Option<App>>,
    }

    // SAFETY: supported VMs have private memory; the guard rejects recursive entry.
    unsafe impl Sync for Instance {}

    static INSTANCE: Instance =
        Instance { active: AtomicBool::new(false), app: UnsafeCell::new(None) };

    #[unsafe(no_mangle)]
    pub extern "C" fn pb_main() {
        if INSTANCE.active.swap(true, Ordering::Acquire) {
            core::arch::wasm32::unreachable();
        }
        // SAFETY: the guard exclusively owns this VM instance's state until return.
        let slot = unsafe { &mut *INSTANCE.app.get() };
        let app = slot.get_or_insert_with(|| {
            // Wasm has no stderr; send panic details to the host before trapping.
            std::panic::set_hook(Box::new(|info| {
                let message = info.to_string();

                // SAFETY: the Wasm context token and message remain valid for this call.
                unsafe {
                    crate::abi::operations::pb_log(
                        crate::abi::invocation::CONTEXT_TOKEN as *mut _,
                        message.as_ptr(),
                        message.len() as u32,
                    );
                }
            }));
            // SAFETY: the linked application supplies the registration symbol.
            unsafe { playbit_app() }
        });
        let (mut context, mut view) =
            Context::guest(&mut app.context).expect("host supplied valid turn events");
        if context.quit_on_last_window(app.quit_on_last_window).is_err() {
            core::arch::wasm32::unreachable();
        }
        if context.attach_files(&app.files).is_err() {
            core::arch::wasm32::unreachable();
        }
        (app.turn)(&mut context, &mut view);
        let _ = context.trace_flush();
        let events = context.event_status();
        let files = context.flush_files();
        if events.and(files).is_err() {
            core::arch::wasm32::unreachable();
        }
        INSTANCE.active.store(false, Ordering::Release);
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    extern crate std;

    use super::*;
    use crate::abi::syscall::{Error, Opcode};
    use crate::ui::{
        CanvasFrames, CanvasItem, CanvasRect, CanvasText, Color, MAX_CANVAS_ITEMS,
        MAX_CANVAS_TEXT_BYTES, TextStyle, ThemeColor, ThemeOverride, ViewEncoder,
    };
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicUsize, Ordering};

    static CREATED: AtomicUsize = AtomicUsize::new(0);
    static DESTROYED: AtomicUsize = AtomicUsize::new(0);
    static TURNS: AtomicUsize = AtomicUsize::new(0);

    struct HostEvent {
        header: abi::Event,
        data: Vec<u8>,
    }

    #[derive(Default)]
    struct ViewHost {
        events: std::collections::VecDeque<HostEvent>,
        deferred_event: Option<HostEvent>,
        frames: Vec<(usize, Vec<u8>)>,
        error: Option<Error>,
        session_info: Option<crate::abi::session::SessionInfo>,
        event_error: Option<Error>,
        trace_batches: Vec<(u64, u64, Vec<u8>)>,
        trace_error: Option<Error>,
        clock: u64,
        poll_deadline: Option<u64>,
    }

    impl ViewHost {
        fn push(&mut self, kind: crate::abi::syscall::EventKind, target: u32, data: &[u8]) {
            if kind == crate::abi::syscall::EventKind::TextChanged {
                let text = core::str::from_utf8(data).expect("valid fixture text");
                let data =
                    crate::abi::text::TextInput::encode(text, &[]).expect("bounded fixture text");
                self.push_raw(kind as u32, target, &data);
            } else {
                self.push_raw(kind as u32, target, data);
            }
        }

        fn push_raw(&mut self, kind: u32, target: u32, data: &[u8]) {
            self.events.push_back(HostEvent {
                header: abi::Event {
                    size: core::mem::size_of::<abi::Event>() as u32,
                    written: core::mem::size_of::<abi::Event>() as u32,
                    generation: 7,
                    kind,
                    target,
                },
                data: data.to_vec(),
            });
        }
    }

    struct Counter(usize);

    #[test]
    fn session_info_distinguishes_absence_seedless_and_an_all_zero_seed() {
        use crate::abi::session::{HAS_KEY_SECRET, SessionInfo};
        let read = |record, error| {
            let mut host = ViewHost { session_info: record, error, ..Default::default() };
            let mut state = ContextState::default();
            // SAFETY: the fixture owns the host during the synchronous callback.
            let (mut context, _) = unsafe { Context::from_raw((&raw mut host).cast(), &mut state) }
                .expect("fixture context");
            context.session_info()
        };
        assert!(matches!(read(None, None), Ok(None)));
        assert!(matches!(read(None, Some(Error::Unsupported)), Err(Error::Unsupported)));
        let mut record = SessionInfo {
            internal_id: [99; 32],
            written: 64,
            flags: 1 << 31,
            ..SessionInfo::default()
        };
        let seedless = read(Some(record), None).expect("session info").expect("session");
        assert_eq!(seedless.internal_id, [99; 32]);
        assert!(seedless.key_secret.is_none() && seedless.code().is_none());
        record.flags |= HAS_KEY_SECRET;
        let zero = read(Some(record), None).expect("session info").expect("zero seed");
        assert_eq!(zero.code().as_deref(), Some("1111111111111111111111"));
        record.key_secret = *b"session-seed-v1!";
        let seeded = read(Some(record), None).expect("session info").expect("seeded session");
        assert_eq!(seeded.code().as_deref(), Some("FFUk4yL7S6oHXq7rrK6a48"));
        record.written = 48;
        assert!(matches!(read(Some(record), None), Err(Error::Internal)));
    }

    impl Drop for Counter {
        fn drop(&mut self) {
            DESTROYED.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[unsafe(no_mangle)]
    fn playbit_app() -> App {
        CREATED.fetch_add(1, Ordering::Relaxed);
        app(
            Counter(0),
            |counter, context, view| {
                counter.0 += 1;
                TURNS.fetch_add(counter.0, Ordering::Relaxed);
                view.column_start();
                let clicked =
                    view.button(1, "Click", crate::ui::ButtonStyle::Default, false).was_clicked();
                assert_eq!(clicked, counter.0 == 2);
                assert_eq!(
                    clicked,
                    context
                        .raw_events()
                        .any(|event| event.kind == crate::abi::syscall::EventKind::Click)
                );
                view.text(TextStyle::Body, "Owned by the SDK");
                view.column_end();
                view.submit(context).expect("valid registered frame");
            },
            true,
        )
    }

    // The SDK unit target has no runtime dependency. Check the scalar boundary
    // with an opaque context, never dereferencing it as a production context.
    #[unsafe(no_mangle)]
    extern "C" fn pb_syscall(
        context: *mut abi::Context,
        operation: u32,
        a: u64,
        b: u64,
        c: u64,
        d: u64,
        e: u64,
        f: u64,
    ) -> i64 {
        assert!(!context.is_null());
        match operation {
            code if code == Opcode::SessionInfo as u32 => {
                // SAFETY: the mock callback lends the live host and one initialized record.
                let (host, output) = unsafe {
                    (
                        &*context.cast::<ViewHost>(),
                        &mut *(a as usize as *mut crate::abi::session::SessionInfo),
                    )
                };
                assert_eq!([b, c, d, e, f], [0; 5]);
                assert_eq!((output.size, output.written), (64, 0));
                if let Some(error) = host.error {
                    return error as i64;
                }
                let Some(record) = host.session_info else { return Error::NotFound as i64 };
                *output = record;
                0
            }
            code if code == Opcode::ClockRead as u32 => {
                // SAFETY: the fixture owns the live context.
                let host = unsafe { &mut *context.cast::<ViewHost>() };
                assert_eq!([a, b, c, d, e, f], [0; 6]);
                host.clock += 1;
                host.clock as i64
            }
            code if code == Opcode::TraceRecord as u32 => {
                // SAFETY: the SDK lends a complete batch and this fixture owns the host.
                let host = unsafe { &mut *context.cast::<ViewHost>() };
                let bytes = unsafe { core::slice::from_raw_parts(a as *const u8, b as usize) };
                host.trace_batches.push((c, d, bytes.to_vec()));
                host.trace_error.map_or(0, |error| error as i64)
            }
            code if code == Opcode::QuitOnLastWindow as u32 => {
                assert_eq!(a, 1);
                assert_eq!([b, c, d, e, f], [0; 5]);
                0
            }
            code if code == Opcode::EventPoll as u32
                || code == Opcode::EventPollDeadline as u32 =>
            {
                // SAFETY: callback fixtures lend a live ViewHost for the whole turn.
                let host = unsafe { &mut *context.cast::<ViewHost>() };
                assert_eq!([e, f], [0; 2]);
                if code == Opcode::EventPollDeadline as u32 {
                    host.poll_deadline = Some(d);
                } else {
                    assert_eq!(d, 0);
                }
                let Some(event) = host.events.front() else {
                    return host.event_error.unwrap_or(Error::WouldBlock) as i64;
                };
                if event.data.len() > c as usize {
                    return event.data.len() as i64;
                }
                let event = host.events.pop_front().expect("front event exists");
                if let Some(deferred) = host.deferred_event.take() {
                    host.events.push_back(deferred);
                }
                // SAFETY: the SDK supplies disjoint initialized header and payload storage.
                unsafe {
                    crate::abi::record::write_native(a as *mut abi::Event, event.header)
                        .expect("event output");
                    core::ptr::copy_nonoverlapping(
                        event.data.as_ptr(),
                        b as *mut u8,
                        event.data.len(),
                    );
                }
                event.data.len() as i64
            }
            code if code == Opcode::ViewSubmit as u32 => {
                assert_eq!([c, d, e, f], [0; 4]);
                // SAFETY: submission tests lend a live ViewHost; the SDK borrows its scene.
                let host = unsafe { &mut *context.cast::<ViewHost>() };
                let bytes = unsafe { core::slice::from_raw_parts(a as *const u8, b as usize) };
                host.frames.push((a as usize, bytes.to_vec()));
                host.error.map_or(0, |error| error as i64)
            }
            other => panic!("unexpected test operation {other:?}"),
        }
    }

    fn snapshot_host() -> ViewHost {
        use crate::abi::syscall::{EventKind, FrameInput, KeyInput, PointerInput};
        let pointer = PointerInput { x: 10, y: 20, phase: 1, buttons: 1 };
        let key = KeyInput { key: 1, pressed: true };
        let frame = FrameInput { elapsed_us: 16_667 };
        let mut host = ViewHost::default();
        for (kind, id, data) in [
            (EventKind::Started, 0, &b""[..]),
            (EventKind::Click, 8, &b""[..]),
            (EventKind::Click, 1, &b""[..]),
            (EventKind::TextChanged, 3, "café".as_bytes()),
            (EventKind::TextChanged, 4, &b"old"[..]),
            (EventKind::Submit, 3, "café".as_bytes()),
            (EventKind::Click, 2, &b""[..]),
            (EventKind::TextChanged, 3, &b""[..]),
            (EventKind::TextChanged, 4, &b"new"[..]),
            (EventKind::Submit, 4, &b"chosen"[..]),
            (EventKind::Click, 1, &b""[..]),
            (EventKind::Click, 9, &b""[..]),
            (EventKind::Click, 12, &b""[..]),
            (EventKind::Click, 11, &b""[..]),
            (EventKind::DataChanged, 0, &b"opaque"[..]),
            (EventKind::Submit, 3, &b""[..]),
            (EventKind::TextChanged, 3, &b"final"[..]),
            (EventKind::Submit, 4, &b"again"[..]),
            (EventKind::Click, 8, &b""[..]),
            (EventKind::Click, 12, &b""[..]),
            (EventKind::Click, 2, &b""[..]),
            (EventKind::Submit, 3, &b"final"[..]),
            (EventKind::Click, 9, &b""[..]),
            (EventKind::Click, 11, &b""[..]),
            (EventKind::Submit, 4, &b"again"[..]),
            (EventKind::TextChanged, 4, &b"new"[..]),
        ] {
            host.push(kind, id, data);
        }
        host.push(EventKind::Pointer, 5, &pointer.encode());
        host.push(EventKind::Key, 5, &key.encode());
        host.push(EventKind::Key, 6, &key.encode());
        host.push(EventKind::Frame, 5, &frame.encode());
        host.push(EventKind::Key, 5, &KeyInput { pressed: false, ..key }.encode());
        host.push(
            EventKind::Pointer,
            5,
            &PointerInput { phase: 3, buttons: 0, ..pointer }.encode(),
        );
        host.push(EventKind::Frame, 6, &frame.encode());
        host.push(EventKind::Frame, 5, &frame.encode());
        host.push(EventKind::DataChanged, 0, b"last");
        host.push(EventKind::VolumesChanged, 0, &42u64.to_le_bytes());

        host
    }

    #[test]
    fn deadline_poll_preserves_clock_width_and_distinguishes_timeout_probe_and_empty_event() {
        use crate::abi::syscall::{EventKind, OPTIONAL_EVENT_KIND_BIT};
        use crate::context::PollError;

        let mut host = ViewHost { clock: 1 << 44, ..ViewHost::default() };
        host.push(EventKind::Started, 0, &[]);
        let mut state = ContextState::default();
        // SAFETY: the fixture owns the mock host and outputs across all synchronous polls.
        let (mut context, _) =
            unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), &mut state) }
                .expect("initial snapshot");
        let deadline = context.clock_read() + 50_000;
        assert_eq!(deadline, (1 << 44) + 50_001);
        assert_eq!(context.raw_events().next().expect("startup event").kind, EventKind::Started);
        let mut bytes = [0; 8];
        assert_eq!(
            context.poll_event_deadline(deadline, &mut bytes),
            Err(PollError::Host(Error::WouldBlock))
        );
        assert_eq!(host.poll_deadline, Some(deadline));

        host.push_raw(OPTIONAL_EVENT_KIND_BIT | 999, 0, &[]);
        host.push(EventKind::TraceConfig, 0, &7u64.to_le_bytes());
        host.push(EventKind::DataChanged, 0, b"payload");
        host.push(EventKind::Click, 1, &[]);
        assert_eq!(
            context.poll_event_deadline(u64::MAX, &mut []),
            Err(PollError::BufferTooSmall(8))
        );
        assert_eq!(host.poll_deadline, Some(u64::MAX));
        let event = context.poll_event_deadline(0, &mut bytes).expect("queued event");
        assert_eq!((event.kind, event.data), (EventKind::DataChanged, b"payload".as_slice()));
        assert!(context.trace_enabled());
        let event = context.poll_event_deadline(0, &mut []).expect("empty event payload");
        assert_eq!((event.target, event.kind), (1, EventKind::Click));

        host.push_raw(999, 0, &[]);
        host.push(EventKind::Click, 2, &[]);
        assert_eq!(
            context.poll_event_deadline(0, &mut bytes),
            Err(PollError::Host(Error::Unsupported))
        );
        assert_eq!(context.event_status(), Err(Error::Unsupported));
        assert_eq!(
            context.poll_event_deadline(0, &mut bytes),
            Err(PollError::Host(Error::Unsupported))
        );
        assert_eq!(host.events.len(), 1);
    }

    fn changed_event(text: &str) -> crate::ui::TextEvent<'_> {
        // All snapshot-only fixtures use the same empty edit list.
        let empty = crate::abi::text::TextInput::decode(&[0; 4]).expect("empty snapshot").edits;
        crate::ui::TextEvent::Changed { text, edits: empty }
    }

    #[test]
    fn text_responses_expose_native_byte_edits_and_explicit_snapshot_availability() {
        // Both field types preserve exact ranges, even where a text diff is ambiguous.
        use crate::abi::{
            syscall::EventKind,
            text::{TextEdit, TextInput},
        };
        use crate::ui::{AutocompleteItem, TextEvent};
        let edits = [TextEdit { start: 2, end: 2, inserted_len: 1 }];
        let data = TextInput::encode("éaaaa", &edits).expect("native edit");
        let mut host = ViewHost::default();
        host.push_raw(EventKind::TextChanged as u32, 1, &data);
        host.push_raw(EventKind::TextChanged as u32, 2, &data);
        host.push_raw(
            EventKind::TextChanged as u32,
            2,
            &TextInput::snapshot("autofill").expect("snapshot"),
        );
        let mut state = ContextState::default();
        // SAFETY: the mock host and snapshot storage outlive every response.
        let (_, mut view) =
            unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), &mut state) }
                .expect("valid edits");
        view.column_start();
        let mut value = String::new();
        let field = view.text_field(1, &mut value, "", false, false, false);
        let TextEvent::Changed { text, edits: Some(delivered) } =
            field.events().next().expect("one edit")
        else {
            panic!("exact edit")
        };
        assert_eq!(text, "éaaaa");
        assert_eq!(delivered.collect::<Vec<_>>(), edits);
        assert_eq!(field.changed_text(), Some("éaaaa"));
        let suggestions = [AutocompleteItem { label: "Choice", value: "choice" }];
        let field = view.autocomplete_field(2, "", "", &suggestions, false);
        assert!(matches!(field.events().next(), Some(TextEvent::Changed { edits: Some(_), .. })));
        assert_eq!(
            field.events().next_back(),
            Some(TextEvent::Changed { text: "autofill", edits: None })
        );
        assert_eq!(field.changed_text(), Some("autofill"));
        view.column_end();
    }

    fn assert_text_snapshot(view: &mut ViewEncoder) {
        use crate::ui::{AutocompleteItem, ButtonStyle, CheckState, TextEvent};
        let button = view.button(1, "Click", ButtonStyle::Default, true);
        assert!(button.was_clicked());
        assert_eq!(button.click_count(), 2);
        let button = view.button(99, "Other", ButtonStyle::Default, false);
        assert!(!button.was_clicked());
        assert_eq!(button.click_count(), 0);
        let mut check = CheckState::Off;
        let checkbox = view.checkbox(2, "Check", &mut check, false);
        assert!(checkbox.was_clicked());
        assert_eq!(checkbox.click_count(), 2);
        assert_eq!(check, CheckState::Off);
        let mut value = String::new();
        let field = view.text_field(3, &mut value, "", false, false, false);
        assert_eq!(value, "final");
        assert!(field.was_changed() && field.was_submitted());
        assert_eq!(field.changed_text(), Some("final"));
        assert_eq!(field.submitted_text(), Some("final"));
        let edits = [
            changed_event("café"),
            TextEvent::Submitted("café"),
            changed_event(""),
            TextEvent::Submitted(""),
            changed_event("final"),
            TextEvent::Submitted("final"),
        ];
        assert_eq!(field.events().collect::<Vec<_>>(), edits);
        assert_eq!(
            field.events().rev().collect::<Vec<_>>(),
            edits.into_iter().rev().collect::<Vec<_>>()
        );
        let field = view.autocomplete_field(
            4,
            "",
            "",
            &[AutocompleteItem { label: "Chosen", value: "chosen" }],
            false,
        );
        assert!(field.was_changed() && field.was_submitted());
        assert_eq!(field.changed_text(), Some("new"));
        assert_eq!(field.submitted_text(), Some("again"));
        assert_eq!(
            field.events().collect::<Vec<_>>(),
            [
                changed_event("old"),
                changed_event("new"),
                TextEvent::Submitted("chosen"),
                TextEvent::Submitted("again"),
                TextEvent::Submitted("again"),
                changed_event("new"),
            ]
        );
    }

    fn assert_action_snapshot(view: &mut ViewEncoder) {
        use crate::ui::{MenuItem, Segment};
        let items = [MenuItem::Submenu {
            label: "Nested",
            icon: None,
            disabled: false,
            items: &[
                MenuItem::Action {
                    id: 11,
                    label: "First",
                    icon: None,
                    checked: None,
                    disabled: false,
                    destructive: false,
                },
                MenuItem::Action {
                    id: 12,
                    label: "Second",
                    icon: None,
                    checked: None,
                    disabled: false,
                    destructive: false,
                },
            ],
        }];
        let segments = [
            Segment { id: 9, label: "Second", disabled: false },
            Segment { id: 8, label: "First and third", disabled: false },
            Segment { id: 7, label: "Unchanged", disabled: false },
        ];
        let control = view.segmented_control(10, &segments, 7, false);
        assert!(control.was_clicked());
        assert_eq!(control.clicked_segments().collect::<Vec<_>>(), [8, 9, 8, 9]);
        assert_eq!(control.clicked_segments().rev().collect::<Vec<_>>(), [9, 8, 9, 8]);
        let popup = view.popup_button(13, "Menu", &items, false);
        assert!(popup.was_clicked());
        assert_eq!(popup.clicked_items().collect::<Vec<_>>(), [12, 11, 12, 11]);
        let menu = view.context_menu_start(&items);
        assert!(menu.was_clicked());
        assert_eq!(menu.clicked_items().collect::<Vec<_>>(), [12, 11, 12, 11]);
        view.text(TextStyle::Body, "Context target");
        view.context_menu_end();
    }

    fn assert_canvas_snapshot(view: &mut ViewEncoder) {
        use crate::abi::syscall::{FrameInput, KeyInput, PointerInput};
        use crate::ui::CanvasEvent;
        // A guest can use its own motion type with the released CanvasFrames parameter.
        struct Motion;
        impl From<Motion> for CanvasFrames {
            fn from(_: Motion) -> Self {
                Self::Continuous
            }
        }
        let pointer = PointerInput { x: 10, y: 20, phase: 1, buttons: 1 };
        let key = KeyInput { key: 1, pressed: true };
        let frame = FrameInput { elapsed_us: 16_667 };
        assert_eq!(
            view.canvas(5, 100, 100, Motion.into(), &[]).events().collect::<Vec<_>>(),
            [
                CanvasEvent::Pointer(pointer),
                CanvasEvent::Key(key),
                CanvasEvent::Frame(frame),
                CanvasEvent::Key(KeyInput { pressed: false, ..key }),
                CanvasEvent::Pointer(PointerInput { phase: 3, buttons: 0, ..pointer }),
                CanvasEvent::Frame(frame),
            ]
        );
        assert_eq!(
            view.canvas(6, 100, 100, CanvasFrames::Continuous, &[]).events().collect::<Vec<_>>(),
            [CanvasEvent::Key(key), CanvasEvent::Frame(frame)]
        );
    }

    #[test]
    fn element_responses_share_an_ordered_repeatable_turn_snapshot() {
        // Interleave targets, repeat actions, and query every response again after submission.
        use crate::abi::syscall::EventKind;
        let mut host = snapshot_host();
        let mut state = ContextState::default();
        // SAFETY: the mock host and event storage outlive this callback.
        let (mut context, mut view) =
            unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), &mut state) }
                .expect("valid mixed events");
        assert!(host.events.is_empty());
        let raw = context.raw_events().collect::<Vec<_>>();
        assert_eq!(raw.len(), 36);
        assert!(raw.iter().all(|event| event.generation == 7));
        assert_eq!(crate::abi::text::TextInput::decode(raw[3].data).expect("edit").text, "café");
        assert_eq!(raw[14].data, b"opaque");
        assert_eq!(raw[34].kind, EventKind::DataChanged);
        assert_eq!(raw[35].kind, EventKind::VolumesChanged);
        assert_eq!(raw[35].data, 42u64.to_le_bytes());

        for _ in 0..2 {
            view.column_start();
            assert_text_snapshot(&mut view);
            assert_action_snapshot(&mut view);
            assert_canvas_snapshot(&mut view);
            view.column_end();
            view.submit(&mut context).expect("valid frame");
            assert_eq!(context.raw_events().collect::<Vec<_>>(), raw);
        }
    }

    #[test]
    fn text_field_updates_state_and_scene_from_the_latest_text_event() {
        // Compare bound state with the first submitted scene across ordered text batches.
        use crate::abi::syscall::EventKind::{Click, Submit, TextChanged};
        use crate::abi::text::MAX_TEXT_BYTES;

        let maximum = "x".repeat(MAX_TEXT_BYTES);
        let cases = [
            (&[][..], "initial"),
            (&[(TextChanged, 2, "other field")][..], "initial"),
            (&[(TextChanged, 1, "café 🦀")][..], "café 🦀"),
            (&[(Submit, 1, "submitted")][..], "submitted"),
            (&[(TextChanged, 1, "edit"), (Submit, 1, "submitted")][..], "submitted"),
            (&[(Submit, 1, "submitted"), (TextChanged, 1, "next draft")][..], "next draft"),
            (&[(TextChanged, 1, "edit"), (TextChanged, 1, ""), (Click, 1, "")][..], ""),
            (&[(Submit, 1, "")][..], ""),
            (&[(TextChanged, 1, maximum.as_str())][..], maximum.as_str()),
        ];
        let mut value = String::with_capacity(MAX_TEXT_BYTES);
        let address = value.as_ptr();
        let mut state = ContextState::default();

        for (disabled, read_only) in [(false, false), (true, false), (false, true)] {
            for (events, expected) in cases {
                let mut host = ViewHost::default();
                for &(kind, target, text) in events {
                    host.push(kind, target, text.as_bytes());
                }
                value.clear();
                value.push_str("initial");
                // SAFETY: the fixture lends the mock host through the synchronous submission.
                let (mut context, mut view) =
                    unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), &mut state) }
                        .expect("valid text batch");
                view.column_start();
                view.text_field(1, &mut value, "", false, disabled, read_only);
                assert_eq!(value, expected);
                assert_eq!(value.as_ptr(), address);
                assert_eq!(context.raw_events().len(), events.len());
                view.column_end();
                view.submit(&mut context).expect("valid text field");

                assert_eq!(host.frames.len(), 1);
                let bytes = &host.frames[0].1;
                assert_eq!(bytes[13], crate::ui::VIEW_TEXT_FIELD);
                assert_eq!(&bytes[23..27], &(expected.len() as u32).to_le_bytes());
                assert_eq!(&bytes[27..bytes.len() - 9], expected.as_bytes());
            }
        }
    }

    #[test]
    fn checkbox_updates_state_and_scene_from_delivered_clicks() {
        // Check bound state and each submitted scene; rebuilding must not replay clicks.
        use crate::abi::syscall::{EventKind, MAX_QUEUED_EVENTS};
        use crate::ui::{CONTROL_DISABLED, CheckState, VIEW_CHECKBOX};
        let mut state = ContextState::default();

        for initial in [CheckState::Off, CheckState::On, CheckState::Mixed] {
            for disabled in [false, true] {
                for clicks in [0, 1, 2, 3, MAX_QUEUED_EVENTS - 2] {
                    let mut host = ViewHost::default();
                    host.push(EventKind::Click, 2, &[]);
                    host.push(EventKind::TextChanged, 1, b"unrelated");
                    for _ in 0..clicks {
                        host.push(EventKind::Click, 1, &[]);
                    }
                    let expected = if clicks == 0 {
                        initial
                    } else if (initial == CheckState::On) == (clicks % 2 == 0) {
                        CheckState::On
                    } else {
                        CheckState::Off
                    };
                    // SAFETY: the fixture lends the mock host through both synchronous submissions.
                    let (mut context, mut view) = unsafe {
                        Context::from_raw((&mut host as *mut ViewHost).cast(), &mut state)
                    }
                    .expect("valid click batch");
                    let mut check = initial;
                    for _ in 0..2 {
                        view.column_start();
                        let checkbox = view.checkbox(1, "Check", &mut check, disabled);
                        assert_eq!(check, expected);
                        for _ in 0..2 {
                            assert_eq!(checkbox.was_clicked(), clicks != 0);
                            assert_eq!(checkbox.click_count(), clicks);
                        }
                        assert_eq!(context.raw_events().len(), clicks + 2);
                        view.column_end();
                        view.submit(&mut context).expect("valid checkbox");
                    }

                    assert_eq!(host.frames.len(), 2);
                    for (_, bytes) in &host.frames {
                        assert_eq!(bytes[13], VIEW_CHECKBOX);
                        assert_eq!(bytes[22], expected as u8);
                        assert_eq!(bytes[23], if disabled { CONTROL_DISABLED } else { 0 });
                    }
                }
            }
        }
    }

    #[test]
    fn event_storage_reuses_payloads_and_does_not_replay_previous_turns() {
        // Refill the same storage with equal and smaller payloads, then an empty turn.
        use crate::abi::syscall::{EventKind, MAX_EVENT_BYTES};
        let mut host = ViewHost::default();
        let mut state = ContextState::default();
        let large = "x".repeat(MAX_EVENT_BYTES);
        let mut address = None;
        for data in [large.as_bytes(), large.as_bytes(), b"short", b""] {
            host.push(EventKind::DataChanged, 1, data);
            // SAFETY: the fixture lends the mock host for each independent turn.
            let (context, _) =
                unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), &mut state) }
                    .expect("bounded text input");
            let event = context.raw_events().next().expect("one event");
            assert_eq!(event.data, data);
            assert_eq!(context.raw_events().len(), 1);
            assert_eq!(*address.get_or_insert(event.data.as_ptr()), event.data.as_ptr());
        }
        host.push(EventKind::Click, 2, &[]);
        host.push(EventKind::Submit, 1, b"submitted");
        // SAFETY: the fixture retains the mock host while this turn is inspected.
        let (context, mut view) =
            unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), &mut state) }
                .expect("click and submission");
        assert_eq!(context.raw_events().len(), 2);
        view.column_start();
        assert_eq!(
            view.button(2, "Click", crate::ui::ButtonStyle::Default, false).click_count(),
            1
        );
        let mut value = String::new();
        assert!(view.text_field(1, &mut value, "", false, false, false).was_submitted());
        assert_eq!(value, "submitted");
        // SAFETY: the same live host now has no queued input.
        let (context, mut view) =
            unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), &mut state) }
                .expect("empty turn");
        assert_eq!(context.raw_events().len(), 0);
        view.column_start();
        let field = view.text_field(1, &mut value, "", false, false, false);
        assert_eq!(value, "submitted");
        assert!(!field.was_changed());
        assert!(!field.was_submitted());
        assert!(field.events().next().is_none());
        let button = view.button(2, "Click", crate::ui::ButtonStyle::Default, false);
        assert!(!button.was_clicked());
        assert_eq!(button.click_count(), 0);
    }

    #[test]
    fn event_prepass_rejects_malformed_and_oversized_events_before_the_callback() {
        // Boundary failures must not deliver a partial batch or call application code.
        use crate::abi::syscall::{EventKind, MAX_EVENT_BYTES};
        let mut state = ContextState::default();
        let raw_kind = |kind, target, data: Vec<u8>| HostEvent {
            header: abi::Event {
                size: core::mem::size_of::<abi::Event>() as u32,
                written: core::mem::size_of::<abi::Event>() as u32,
                generation: 7,
                kind,
                target,
            },
            data,
        };
        for event in [
            raw_kind(EventKind::Click as u32, 0, Vec::new()),
            raw_kind(EventKind::Click as u32, 1, vec![1]),
            raw_kind(EventKind::TextChanged as u32, 1, vec![0xff]),
            raw_kind(EventKind::Pointer as u32, 1, vec![0; 15]),
            raw_kind(EventKind::Key as u32, 1, vec![0; 3]),
            raw_kind(EventKind::Frame as u32, 1, vec![0; 3]),
            raw_kind(EventKind::TextChanged as u32, 1, vec![b'x'; MAX_EVENT_BYTES + 1]),
        ] {
            let mut host = ViewHost::default();
            host.push(EventKind::Click, 1, &[]);
            host.events.push_back(event);
            // SAFETY: malformed wire values remain inside valid mock allocations.
            assert!(
                unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), &mut state) }
                    .is_err()
            );
            assert_eq!(state.raw_events().len(), 0);
        }
        let mut host = ViewHost { event_error: Some(Error::Unavailable), ..ViewHost::default() };
        host.push(EventKind::Click, 1, &[]);
        let mut application = App {
            turn: Box::new(|_, _| panic!("a failed prepass must not enter the app")),
            context: state,
            files: Default::default(),
            quit_on_last_window: true,
        };
        // SAFETY: the real SDK callback borrows both live fixtures synchronously.
        assert_eq!(
            unsafe {
                turn((&mut host as *mut ViewHost).cast(), (&mut application as *mut App).cast())
            },
            Error::Unavailable as i32
        );
        assert_eq!(application.context.raw_events().len(), 0);
    }

    #[test]
    fn unknown_optional_events_preserve_order_and_charge_both_turn_budgets() {
        use crate::abi::syscall::{
            EventKind, MAX_EVENT_BYTES, MAX_QUEUED_EVENT_BYTES, MAX_QUEUED_EVENTS,
            OPTIONAL_EVENT_KIND_BIT,
        };
        let optional = OPTIONAL_EVENT_KIND_BIT | 999;
        let mut host = ViewHost::default();
        let mut state = ContextState::default();
        host.push(EventKind::ObjectCreationReady, 0, &[7; 8]);
        host.push_raw(optional, 0, &[]);
        host.push_raw(optional, 0, &vec![3; MAX_EVENT_BYTES]);
        host.push(EventKind::Click, 1, &[]);
        // SAFETY: the mock host lends a valid context and disjoint output records.
        let (context, _) =
            unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), &mut state) }
                .expect("optional events do not fail the turn");
        assert_eq!(context.event_status(), Ok(()));
        assert_eq!(
            context.raw_events().map(|event| event.kind).collect::<Vec<_>>(),
            [EventKind::ObjectCreationReady, EventKind::Click]
        );
        assert_eq!(context.raw_events().next().expect("retained completion").data, &[7; 8]);
        assert!(host.events.is_empty());

        for _ in 0..MAX_QUEUED_EVENTS {
            host.push_raw(optional, 0, &[]);
        }
        host.push(EventKind::Click, 1, &[]);
        // SAFETY: the same live fixture owns all memory.
        let (context, _) =
            unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), &mut state) }
                .expect("bounded skipped count");
        assert_eq!(context.raw_events().len(), 0);
        assert_eq!(host.events.len(), 1, "skips count toward the turn limit");
        host.events.clear();
        for _ in 0..=MAX_QUEUED_EVENT_BYTES / (MAX_EVENT_BYTES + 20) {
            host.push_raw(optional, 0, &vec![0; MAX_EVENT_BYTES]);
        }
        host.push(EventKind::Click, 1, &[]);
        // SAFETY: the same live fixture owns all memory.
        let (context, _) =
            unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), &mut state) }
                .expect("bounded skipped payload");
        assert_eq!(context.raw_events().len(), 0);
        assert_eq!(host.events.len(), 2, "payloads and queue overhead both count");
    }

    #[test]
    fn unknown_mandatory_event_delivers_prior_completions_then_stops_explicitly() {
        use crate::abi::syscall::{EventKind, OPTIONAL_EVENT_KIND_BIT};
        for kind in [999, OPTIONAL_EVENT_KIND_BIT | EventKind::ObjectCreationReady as u32] {
            let mut host = ViewHost::default();
            host.push(EventKind::ObjectCreationReady, 0, &[7; 8]);
            host.push_raw(kind, 0, &[]);
            host.push(EventKind::Click, 1, &[]);
            let mut application = App {
                turn: Box::new(|context, _| {
                    let events = context.raw_events().collect::<Vec<_>>();
                    assert_eq!(events.len(), 1);
                    assert_eq!(events[0].kind, EventKind::ObjectCreationReady);
                    assert_eq!(events[0].data, &[7; 8]);
                    assert_eq!(context.event_status(), Err(Error::Unsupported));
                    assert_eq!(
                        context.poll_event_deadline(u64::MAX, &mut []),
                        Err(crate::context::PollError::Host(Error::Unsupported))
                    );
                }),
                context: ContextState::default(),
                files: Default::default(),
                quit_on_last_window: true,
            };
            // SAFETY: the real SDK callback borrows both live fixtures synchronously.
            assert_eq!(
                unsafe {
                    turn((&mut host as *mut ViewHost).cast(), (&mut application as *mut App).cast())
                },
                Error::Unsupported as i32
            );
            assert_eq!(host.poll_deadline, None, "a terminal callback cannot park");
            assert_eq!(host.events.len(), 1, "no later event is admitted");
            // SAFETY: a terminal snapshot never polls the live host again.
            assert!(matches!(
                unsafe {
                    Context::from_raw((&mut host as *mut ViewHost).cast(), &mut application.context)
                },
                Err(Error::Unsupported)
            ));
            assert_eq!(host.events.len(), 1);
        }
        let mut host = ViewHost::default();
        host.push_raw(999, 0, &[]);
        let mut state = ContextState::default();
        // SAFETY: a first mandatory unknown has no admitted snapshot to deliver.
        assert!(matches!(
            unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), &mut state) },
            Err(Error::Unsupported)
        ));
    }

    fn assert_bounded_event_count(state: &mut ContextState) {
        use crate::abi::syscall::{EventKind, MAX_QUEUED_EVENTS};
        let revision = 42u64.to_le_bytes();
        for deferred in [false, true] {
            let mut host = ViewHost::default();
            for _ in 0..MAX_QUEUED_EVENTS {
                host.push(EventKind::Click, 1, &[]);
            }
            if deferred {
                host.deferred_event = Some(HostEvent {
                    header: abi::Event {
                        size: core::mem::size_of::<abi::Event>() as u32,
                        written: core::mem::size_of::<abi::Event>() as u32,
                        generation: 7,
                        kind: EventKind::VolumesChanged as u32,
                        target: 0,
                    },
                    data: revision.to_vec(),
                });
            }
            // SAFETY: the mock host stays live for the whole prepass.
            {
                let (context, mut view) =
                    unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), state) }
                        .expect("full click batch");
                assert_eq!(context.raw_events().len(), MAX_QUEUED_EVENTS);
                view.column_start();
                let segments = [
                    crate::ui::Segment { id: 1, label: "Repeated", disabled: false },
                    crate::ui::Segment { id: 2, label: "Untouched", disabled: false },
                    crate::ui::Segment { id: 3, label: "Unselected", disabled: false },
                ];
                let control = view.segmented_control(4, &segments, 1, false);
                assert_eq!(control.clicked_segments().count(), MAX_QUEUED_EVENTS);
                assert!(control.clicked_segments().all(|id| id == 1));
            }
            assert_eq!(host.events.len(), usize::from(deferred));
            // SAFETY: the same host and reusable snapshot remain live for the next turn.
            let (context, _) =
                unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), state) }
                    .expect("deferred notification");
            assert_eq!(context.raw_events().len(), usize::from(deferred));
            if let Some(event) = context.raw_events().next() {
                assert_eq!(event.kind, EventKind::VolumesChanged);
                assert_eq!(event.data, revision);
            }
            assert!(host.events.is_empty());
        }
    }

    fn assert_bounded_event_bytes(state: &mut ContextState) {
        use crate::abi::syscall::{EventKind, MAX_EVENT_BYTES, MAX_QUEUED_EVENT_BYTES};
        let revision = 42u64.to_le_bytes();
        for (kind, target, data) in
            [(EventKind::Click, 2, &[][..]), (EventKind::VolumesChanged, 0, revision.as_slice())]
        {
            for spare in [0, 19, 20, 27, 28] {
                let mut host = ViewHost::default();
                let full = vec![b'x'; MAX_EVENT_BYTES];
                let full_count = MAX_QUEUED_EVENT_BYTES / (MAX_EVENT_BYTES + 20);
                for _ in 0..full_count {
                    host.push(EventKind::DataChanged, 1, &full);
                }
                let last = MAX_QUEUED_EVENT_BYTES
                    - (full_count + 1) * 20
                    - full_count * MAX_EVENT_BYTES
                    - spare;
                host.push(EventKind::DataChanged, 1, &full[..last]);
                host.deferred_event = Some(HostEvent {
                    header: abi::Event {
                        size: core::mem::size_of::<abi::Event>() as u32,
                        written: core::mem::size_of::<abi::Event>() as u32,
                        generation: 7,
                        kind: kind as u32,
                        target,
                    },
                    data: data.to_vec(),
                });
                let deferred = usize::from(spare < 20 + data.len());
                // SAFETY: every payload fits the host limit; both callbacks borrow this fixture.
                let (context, _) =
                    unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), state) }
                        .expect("bounded payload batch");
                assert_eq!(context.raw_events().len(), full_count + 2 - deferred);
                assert_eq!(
                    context.raw_events().nth(full_count).expect("last edit").data,
                    &full[..last]
                );
                assert_eq!(host.events.len(), deferred);
                // SAFETY: no context borrow from the previous turn remains in use.
                let (context, _) =
                    unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), state) }
                        .expect("remaining payload");
                assert_eq!(context.raw_events().len(), deferred);
                if let Some(event) = context.raw_events().next() {
                    assert_eq!((event.kind, event.target, event.data), (kind, target, data));
                }
                assert!(host.events.is_empty());
            }
        }
    }

    #[test]
    fn event_prepass_bounds_batches_without_consuming_deferred_events() {
        // The host can enqueue a deferred notification as consuming input frees queue space.
        let mut state = ContextState::default();
        assert_bounded_event_count(&mut state);
        assert_bounded_event_bytes(&mut state);
    }

    fn encode_frame(view: &mut ViewEncoder, text: &str, themed: bool) {
        if themed {
            view.theme(&[ThemeOverride { role: ThemeColor::Text, color: Color(0xff0000ff) }]);
        }
        view.column_start();
        view.scroll_start(1, false);
        view.column_start();
        view.text(TextStyle::Body, text);
        view.column_end();
        view.scroll_end();
        view.column_end();
    }

    #[test]
    fn view_submission_reuses_storage_and_resets_after_success_or_rejection() {
        // Alternate frame sizes and themes, then reject a frame before rendering again.
        let mut host = ViewHost::default();
        let mut context_state = ContextState::default();
        // SAFETY: the mock host lives through every synchronous submission.
        let (mut context, mut view) =
            unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), &mut context_state) }
                .expect("mock context is nonnull");
        let large_text = "🦋".repeat(4096);
        for (text, themed, error) in [
            ("First", true, None),
            (large_text.as_str(), false, None),
            ("Rejected", true, Some(Error::LimitExceeded)),
            (large_text.as_str(), true, None),
            ("Last", false, None),
        ] {
            host.error = error;
            encode_frame(&mut view, text, themed);
            assert_eq!(view.submit(&mut context), error.map_or(Ok(()), Err));

            let mut expected_state = crate::context::ContextState::default();
            let mut expected = expected_state.view();
            encode_frame(&mut expected, text, themed);
            assert_eq!(host.frames.last().expect("submitted frame").1, expected.finish());
        }

        assert_eq!(host.frames.len(), 5);
        let pointer = host.frames[1].0;
        for (address, _) in &host.frames[2..] {
            assert_eq!(*address, pointer);
        }
    }

    #[test]
    fn view_submission_resets_canvas_limits_after_success_or_rejection() {
        // Each frame exhausts both scene-wide limits before the encoder is reused.
        let mut host = ViewHost::default();
        let mut context_state = ContextState::default();
        // SAFETY: the mock host lives through every synchronous submission.
        let (mut context, mut view) =
            unsafe { Context::from_raw((&mut host as *mut ViewHost).cast(), &mut context_state) }
                .expect("mock context is nonnull");
        let text = "x".repeat(4096);
        let rect = CanvasRect { x: 0, y: 0, width: 16, height: 16, color: Color(0xffffffff) };
        let labels = alloc::vec![
            CanvasItem::Text(CanvasText { bounds: rect, font_size: 12, bold: false, text: &text });
            MAX_CANVAS_TEXT_BYTES / text.len()
        ];
        let rects = alloc::vec![rect; MAX_CANVAS_ITEMS - labels.len()];

        for error in [None, Some(Error::LimitExceeded), None] {
            host.error = error;
            view.column_start();
            view.canvas(1, 16, 16, CanvasFrames::Continuous, &labels);
            view.canvas_static(16, 16, &rects);
            view.column_end();
            let length = view.byte_len();
            assert_eq!(view.submit(&mut context), error.map_or(Ok(()), Err));
            assert_eq!(host.frames.last().expect("submitted frame").1.len(), length);
        }

        assert_eq!(host.frames.len(), 3);
        assert!(host.frames.windows(2).all(|frames| frames[0] == frames[1]));
    }

    #[test]
    fn view_submission_panics_before_calling_the_host_for_an_invalid_frame() {
        // An absent, unclosed, or extra root must never cross the host boundary.
        let mut host = ViewHost::default();
        let mut context_state = crate::context::ContextState::default();
        for (roots, closed) in [(0, true), (1, false), (2, true)] {
            // SAFETY: the mock host lives through every synchronous submission.
            let (mut context, mut view) = unsafe {
                Context::from_raw((&mut host as *mut ViewHost).cast(), &mut context_state)
            }
            .expect("mock context is nonnull");
            for _ in 0..roots {
                view.column_start();
                if closed {
                    view.column_end();
                }
            }
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _ = view.submit(&mut context);
                }))
                .is_err()
            );
        }
        assert!(host.frames.is_empty());
    }

    #[test]
    fn native_export_admits_capacity_before_publishing_callbacks() {
        #[repr(C, align(8))]
        struct Header {
            size: u32,
            written: u32,
        }
        #[repr(C)]
        struct Future {
            app: abi::App,
            tail: [u8; 16],
        }
        let mut short = Header { size: 8, written: 0 };
        let mut future = Future { app: native_app(), tail: [0xa5; 16] };
        future.app.size = core::mem::size_of::<Future>() as u32;
        // SAFETY: both fixtures lend exactly the declared storage; the small
        // header must be rejected without accessing nonexistent callback fields.
        unsafe {
            assert_eq!(native_app_export(core::ptr::null_mut()), Error::InvalidArgument as i32);
            assert_eq!(
                native_app_export((&mut short as *mut Header).cast()),
                Error::BufferTooSmall as i32
            );
            assert_eq!(native_app_export(&mut future.app), 0);
            assert_eq!(future.app.size, core::mem::size_of::<Future>() as u32);
            assert_eq!(future.app.written, core::mem::size_of::<abi::App>() as u32);
            assert!(
                future.app.init.is_some()
                    && future.app.turn.is_some()
                    && future.app.destroy.is_some()
            );
            assert_eq!(future.tail, [0xa5; 16]);
        }
    }

    #[test]
    fn native_registration_owns_independent_instances_and_failed_init_is_destroyable() {
        // Drive the real callbacks and scalar intent declaration with a mock context.
        let descriptor = native_app();
        let init = descriptor.init.expect("SDK init");
        let turn = descriptor.turn.expect("SDK turn");
        let destroy = descriptor.destroy.expect("SDK destroy");
        let mut context = ViewHost::default();
        let raw = (&mut context as *mut ViewHost).cast();
        let mut first = core::ptr::null_mut();
        let mut second = core::ptr::null_mut();

        // SAFETY: outputs and mock context are live; successful states are destroyed once.
        unsafe {
            assert_eq!(init(raw, descriptor.user_data, &mut first), 0);
            assert_eq!(init(raw, descriptor.user_data, &mut second), 0);
            assert_ne!(first, second);
            assert_eq!(CREATED.load(Ordering::Relaxed), 2);
            assert_eq!(turn(raw, first), 0);
            context.push(crate::abi::syscall::EventKind::Click, 1, &[]);
            assert_eq!(turn(raw, first), 0);
            assert_eq!(turn(raw, second), 0);
            assert_eq!(TURNS.load(Ordering::Relaxed), 4);
            assert_eq!(context.frames.len(), 3);
            assert_eq!(context.frames[0].0, context.frames[1].0);
            assert_ne!(context.frames[0].0, context.frames[2].0);
            assert_ne!(turn(core::ptr::null_mut(), first), 0);
            assert_ne!(turn(raw, core::ptr::null_mut()), 0);
            destroy(first);
            destroy(second);
            assert_eq!(DESTROYED.load(Ordering::Relaxed), 2);

            let mut failed = core::ptr::dangling_mut();
            assert_ne!(init(core::ptr::null_mut(), descriptor.user_data, &mut failed), 0);
            assert!(failed.is_null());
            destroy(failed);
            assert_ne!(init(raw, descriptor.user_data, core::ptr::null_mut()), 0);
            assert_eq!(CREATED.load(Ordering::Relaxed), 2);
            assert_eq!(DESTROYED.load(Ordering::Relaxed), 2);
        }
    }

    #[test]
    fn spans_cross_callbacks_end_out_of_order_and_reject_stale_generations() {
        // Keep 400 tokens across callback snapshots, reverse their ends, then restart.
        use crate::abi::trace;
        let initial_clock = 1u64 << 63;
        let mut host = ViewHost { clock: initial_clock, ..Default::default() };
        let mut state = ContextState::default();
        host.push(crate::abi::syscall::EventKind::TraceConfig, 0, &7u64.to_le_bytes());
        let raw = (&mut host as *mut ViewHost).cast();
        // SAFETY: this fixture owns the live host and serializes all callbacks.
        let (mut context, _) =
            unsafe { Context::from_raw(raw, &mut state) }.expect("first callback");
        let mut spans = Vec::new();
        for _ in 0..400 {
            spans.push(
                context
                    .trace_begin(0, None, br#"{"actor_type":"monster","position":{"x":12.5}}"#)
                    .expect("start"),
            );
        }
        context.trace_flush().expect("callback flush");
        drop(context);
        let (mut context, _) =
            unsafe { Context::from_raw(raw, &mut state) }.expect("second callback");
        let parent = spans[0];
        context
            .trace_instant(
                1,
                u64::MAX,
                Some(parent),
                br#"{"network_location":{"host":"game.example","port":443}}"#,
            )
            .expect("instant");
        for span in spans.iter().rev() {
            context.trace_end(*span, br#"{"ok":true}"#).expect("end");
        }
        assert_eq!(
            context.trace_begin(0, None, &[b'x'; trace::MAX_METADATA_BYTES + 1]),
            Err(Error::LimitExceeded)
        );
        context.trace_flush().expect("flush");
        drop(context);
        let batches = host.trace_batches.len();
        let events: Vec<_> = host
            .trace_batches
            .iter()
            .flat_map(|(generation, _, bytes)| {
                assert_eq!(*generation, 7);
                let mut rest = bytes.as_slice();
                core::iter::from_fn(move || {
                    if rest.is_empty() {
                        return None;
                    }
                    let header = rest.get(..trace::HEADER_BYTES).expect("event header");
                    let metadata_len =
                        u32::from_le_bytes(header[28..32].try_into().expect("metadata length"));
                    let extent = trace::event_bytes(metadata_len as usize).expect("event size");
                    let (_, tail) = rest.split_at_checked(extent).expect("complete event");
                    rest = tail;
                    Some((
                        u64::from_le_bytes(header[..8].try_into().expect("timestamp")),
                        u64::from_le_bytes(header[8..16].try_into().expect("reference")),
                        u32::from_le_bytes(header[24..28].try_into().expect("thread")),
                    ))
                })
            })
            .collect();
        assert_eq!(events.len(), 801);
        assert_eq!(events[0].0, initial_clock + 1, "clock result retains its high bit");
        assert_eq!(host.trace_batches.last().expect("producer loss").1, 1);
        assert_eq!(host.clock - initial_clock, 801, "one clock read per emitted event");
        assert!(batches < 10, "record syscalls batch complete events");
        assert_eq!(events[400].1, parent.timestamp());
        assert_eq!(events[401].1, spans[399].timestamp());
        assert!(events.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert_eq!(events[401].2, 0);
        host.push(crate::abi::syscall::EventKind::TraceConfig, 0, &8u64.to_le_bytes());
        let (mut context, _) = unsafe { Context::from_raw(raw, &mut state) }.expect("new capture");
        assert_eq!(context.trace_end(parent, &[]), Err(Error::StaleHandle));
        context.trace_flush().expect("loss-only batch");
        context.trace_flush().expect("empty flush");
        drop(context);
        assert_eq!(host.trace_batches.len(), batches + 1);
        let (generation, lost, bytes) = host.trace_batches.last().expect("loss-only submission");
        assert_eq!((*generation, *lost), (8, 1));
        assert!(bytes.is_empty());
        host.push(crate::abi::syscall::EventKind::TraceConfig, 0, &0u64.to_le_bytes());
        let (mut context, _) = unsafe { Context::from_raw(raw, &mut state) }.expect("disabled");
        assert_eq!(context.trace_begin(0, None, &[]), Err(Error::Unavailable));
        context.trace_flush().expect("disabled flush");
        drop(context);
        assert_eq!(host.trace_batches.len(), batches + 1);
    }

    #[test]
    fn registration_preserves_overaligned_state() {
        // Typed dispatch and destruction must preserve the concrete state alignment.
        #[repr(align(256))]
        struct Aligned(u32);
        impl Drop for Aligned {
            fn drop(&mut self) {
                assert_eq!(self.0, 8);
            }
        }
        let mut app = app(Aligned(7), |value, _, _| value.0 += 1, true);
        let mut raw = ViewHost::default();
        // SAFETY: this opaque mock context is live and the callback makes no host calls.
        let mut context_state = crate::context::ContextState::default();
        let (mut context, mut view) =
            unsafe { Context::from_raw((&mut raw as *mut ViewHost).cast(), &mut context_state) }
                .expect("non-null mock context");
        (app.turn)(&mut context, &mut view);
        drop(app);
    }
}
