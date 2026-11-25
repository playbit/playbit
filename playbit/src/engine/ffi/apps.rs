//! Borrowed C native-app lifecycle and output delivery under the engine owner lock.
use super::*;
use crate::runtime::syscall::{Event, EventKind};
use playbit_api::abi::operations as abi;
use std::time::Instant;
#[derive(Clone, Copy)]
#[repr(C)]
pub struct AppOutput {
    pub size: u32,
    pub written: u32,
    pub view: Option<unsafe extern "C" fn(*mut c_void, u64, *const u8, u32)>,
    pub log: Option<unsafe extern "C" fn(*mut c_void, u64, *const u8, u32)>,
    pub user_data: *mut c_void,
}
#[derive(Clone, Copy)]
#[repr(C)]
pub struct AppStep {
    pub size: u32,
    pub written: u32,
    pub wake_after_ns: u64,
    pub flags: u32,
    pub reserved: u32,
}
memory::record!(AppOutput, if cfg!(target_pointer_width = "64") { 32 } else { 20 }, {
    view: Option<unsafe extern "C" fn(*mut c_void, u64, *const u8, u32)>,
    log: Option<unsafe extern "C" fn(*mut c_void, u64, *const u8, u32)>,
    user_data: *mut c_void,
});
memory::record!(AppStep, 24, { wake_after_ns: u64, flags: u32, reserved: u32 });
fn step(inner: &Inner, generation: u64) -> Result<AppStep> {
    let step = inner.apps.step(generation, Instant::now())?;
    Ok(AppStep {
        size: std::mem::size_of::<AppStep>() as u32,
        written: 0,
        wake_after_ns: step.wake_after.map_or(0, |duration| {
            u64::try_from(duration.as_nanos()).expect("native wake delay is bounded")
        }),
        flags: u32::from(step.ready) | (u32::from(step.wake_after.is_some()) << 1),
        reserved: 0,
    })
}
/// # Safety
/// Live engine and disjoint fixed records obey playbit/native.h and playbit/sys/abi.h.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_engine_app_attach(
    engine: *mut Engine,
    root: *const [u8; 16],
    app: *const abi::App,
    output: *mut u64,
) -> i32 {
    // SAFETY: all foreign storage is borrowed and admitted before ownership changes.
    unsafe {
        with_engine(engine, |inner| {
            let target = fixed(output)?;
            disjoint(&target, &fixed(root)?)?;
            disjoint(&target, &memory::native_region(app, false)?)?;
            let app = memory::read_native(app, true)?;
            let generation =
                inner.apps.attach(&inner.services, Id::from_bytes(root.read()), app)?;
            output.write(generation);
            Ok(())
        })
    }
}
/// # Safety
/// The live engine follows playbit/native.h, including callback destruction lifetime.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_engine_app_detach(engine: *mut Engine, generation: u64) -> i32 {
    // SAFETY: the engine lock excludes callbacks reentering while destruction runs.
    unsafe { with_engine(engine, |inner| inner.apps.detach(generation)) }
}
/// # Safety
/// Live engine and stable event/payload obey playbit/native.h.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_engine_app_input(
    engine: *mut Engine,
    event: *const abi::Event,
    data: *const u8,
    length: u32,
) -> i32 {
    // SAFETY: copied event/payload remain bounded; empty data may be null.
    unsafe {
        with_engine(engine, |inner| {
            let event = memory::read_native(event, true)?;
            if length > playbit_api::abi::syscall::MAX_EVENT_BYTES as u32 {
                return Err(Error::LimitExceeded);
            }
            let kind = match event.kind {
                2 => EventKind::Click,
                3 => EventKind::Submit,
                11 => EventKind::Pointer,
                12 => EventKind::Key,
                13 => EventKind::Frame,
                14 => EventKind::TextChanged,
                15 => EventKind::SurfaceChanged,
                22 => EventKind::WindowPointer,
                23 => EventKind::WindowKeyboard,
                24 => EventKind::WindowScroll,
                25 => EventKind::WindowGesture,
                26 => EventKind::WindowGamepad,
                _ => return Err(Error::InvalidArgument),
            };
            let bytes = if length == 0 {
                vec![]
            } else {
                input(data, length, playbit_api::abi::syscall::MAX_EVENT_BYTES as u32)?;
                std::slice::from_raw_parts(data, length as usize).to_vec()
            };
            inner.apps.input(
                event.generation,
                Event { object_id: event.object_id, kind, target: event.target, data: bytes },
            )
        })
    }
}
/// # Safety
/// All records/callbacks obey playbit/native.h; output is exclusive and disjoint.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_engine_app_advance(
    engine: *mut Engine,
    generation: u64,
    callbacks: *const AppOutput,
    output: *mut AppStep,
) -> i32 {
    // SAFETY: outputs admitted before executing foreign code or consuming input.
    unsafe {
        with_engine(engine, |inner| {
            disjoint(
                &memory::native_region(output, true)?,
                &memory::native_region(callbacks, false)?,
            )?;
            memory::clear_native(output)?;
            let callbacks = memory::read_native(callbacks, true)?;
            let view = callbacks.view.ok_or(Error::InvalidArgument)?;
            let log = callbacks.log.ok_or(Error::InvalidArgument)?;
            inner.apps.advance(generation, |frame| {
                for text in frame.logs() {
                    log(callbacks.user_data, generation, text.as_ptr(), text.len() as u32);
                }
                if let Some(bytes) = frame.view_bytes() {
                    view(callbacks.user_data, generation, bytes.as_ptr(), bytes.len() as u32);
                }
            })?;
            memory::write_native(output, step(inner, generation)?)?;
            Ok(())
        })
    }
}
/// # Safety
/// Live engine and exclusive fixed output obey playbit/native.h.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_engine_app_step(
    engine: *mut Engine,
    generation: u64,
    output: *mut AppStep,
) -> i32 {
    // SAFETY: result is computed before any output change.
    unsafe {
        with_engine(engine, |inner| {
            memory::native_region(output, true)?;
            memory::clear_native(output)?;
            memory::write_native(output, step(inner, generation)?)?;
            Ok(())
        })
    }
}
