//! Consume variable results through the safe SDK using real wasm32 layouts.
use playbit_api::{
    abi::{data, environment},
    context::{
        Completion, Context, FileHandle, FileResources, OperationHandle, PollError, ReadError,
        ValueRef,
    },
};
fn run() -> Option<()> {
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = Context::guest(&mut context_state).expect("valid events");
    context.attach_files(&FileResources::default()).ok()?;
    let mut names = [data::Range::default(); 32];
    let mut text = [0; 4096];
    if context.capabilities(&mut names, &mut text).ok()?.names().count() == 0 {
        return None;
    }
    let mut arguments = [environment::Argument::default(); 1];
    if context.startup_arguments(&mut arguments, &mut text).ok()?.entries().next()?
        != ("mode", "demo")
    {
        return None;
    }
    let mut participants = [environment::Participant::default(); 1];
    if context.presence(&mut participants).ok()?[0].build_id != [5; 32] {
        return None;
    }
    let mut volumes = [data::Volume::default(); 1];
    let mut tags = [data::Range::default(); 1];
    {
        let result = context.available_volumes(&mut volumes, &mut tags, &mut text).ok()?;
        let volume = result.iter().next()?;
        if volume.name != "Root" || volume.tags().next()? != "test" || !volume.is_root {
            return None;
        }
    };
    context.flush_files().ok()?;
    Some(())
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_results() -> u32 {
    if run().is_some() { 0 } else { 1 }
}

fn begin(generation: u64, volume_slot: u32, kind: u32) -> Option<u64> {
    let volume = data::VolumeHandle::new(generation.try_into().ok()?, volume_slot);
    let entry = playbit_api::context::EntryHandle { volume: volume.0, entry_id: [5; 15], ..Default::default() };
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = Context::guest(&mut context_state).expect("valid events");
    let file = FileHandle(volume.0);
    let bytes = [b'n', b'e', b'w'];
    let operation = match kind {
        0 => context.begin_read_object(entry, [9; 16], &[[7; 16]]),
        1 => context.begin_read_file(file, 0, 64),
        2 => context.begin_write_file(file, 0, &bytes),
        _ => return None,
    }
    .ok()?;
    Some(operation.0)
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_result_begin(generation: u64, volume_slot: u32, kind: u32) -> u64 {
    begin(generation, volume_slot, kind).unwrap_or(0)
}
fn take(generation: u64, slot: u32, kind: u32, _edited: u32) -> Result<Completion<()>, i32> {
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = Context::guest(&mut context_state).expect("valid events");
    let operation = OperationHandle::new(generation.try_into().map_err(|_| -3)?, slot);
    let mut text = [0; 64];
    match kind {
        0 => {
            match context.take_read_object(operation, &mut [], &mut [], &mut []) {
                Ok(Completion::Pending) => return Ok(Completion::Pending),
                Err(ReadError::BufferTooSmall { records: 1, values: 1, bytes: 3 }) => {}
                Err(ReadError::Host(error)) => return Err(error as i32),
                _ => return Err(-4),
            }
            let mut fields = [data::Field::default(); 1];
            let mut values = [data::Value::default(); 1];
            let Completion::Ready(object) = context
                .take_read_object(operation, &mut fields, &mut values, &mut text)
                .map_err(|_| -4)?
            else {
                return Err(-4);
            };
            let text_matches = object.field([7; 16]).is_some_and(|field| {
                matches!(
                    field.values().and_then(|mut values| values.next()),
                    Some(ValueRef::Text("abc"))
                )
            });
            if object.type_id != [8; 16] || !text_matches {
                return Err(-4);
            }
        }
        1 => {
            let count = 5;
            match context.take_read_file(operation, &mut []) {
                Ok(Completion::Pending) => return Ok(Completion::Pending),
                Err(PollError::BufferTooSmall(bytes)) if bytes == count => {}
                Err(PollError::Host(error)) => return Err(error as i32),
                _ => return Err(-4),
            }
            let Completion::Ready(chunk) =
                context.take_read_file(operation, &mut text).map_err(|_| -4)?
            else {
                return Err(-4);
            };
            if chunk != b"hello" {
                return Err(-4);
            }
        }
        2 => return context.take_write_file(operation).map_err(|error| error as i32),
        _ => return Err(-3),
    }
    Ok(Completion::Ready(()))
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_result_take(generation: u64, slot: u32, kind: u32, edited: u32) -> i32 {
    match take(generation, slot, kind, edited) {
        Ok(Completion::Ready(())) => 0,
        Ok(Completion::Pending) => -6,
        Err(error) => error,
    }
}
