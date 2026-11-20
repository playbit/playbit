//! Begin owns compiler-laid-out mutation inputs before the export returns.
use playbit_api::abi::syscall::Error;
use playbit_api::context::{
    Completion, Context, FieldChange, OperationHandle, Value, VolumeHandle,
};
#[unsafe(no_mangle)]
pub extern "C" fn probe_memory_begin(volume: u64, expected_platform: i32, edit: u32) -> u64 {
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = Context::guest(&mut context_state).expect("valid events");
    if context.host_platform().map(|platform| platform as i32) != Ok(expected_platform) || edit > 1
    {
        return 0;
    }
    let Ok(identity) = context.identity() else { return 0 };
    if identity.account_id != [1; 16] {
        return 0;
    }
    let volume = VolumeHandle(volume);
    let entry = playbit_api::context::EntryHandle { volume: volume.0, entry_id: [5; 15], ..Default::default() };
    let values = if edit == 0 {
        [Value::text("compiled").expect("short text")]
    } else {
        [Value::boolean(true)]
    };
    let changes = [FieldChange::set([7; 16], &values).expect("one value")];
    let operation = if edit == 0 {
        context.begin_create_object(entry, [8; 16], &changes)
    } else {
        context.begin_edit_object(entry, [9; 16], &changes)
    };
    operation.map_or(0, |handle| handle.0)
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_memory_take(generation: u32, slot: u32, edit: u32) -> i32 {
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = Context::guest(&mut context_state).expect("valid events");
    let operation = OperationHandle::new(generation, slot);
    if edit != 0 {
        return match context.take_edit_object(operation) {
            Ok(Completion::Pending) => Error::WouldBlock as i32,
            Ok(Completion::Ready(())) => 0,
            Err(error) => error as i32,
        };
    }
    match context.poll_create_object(operation) {
        Ok(Completion::Pending) => Error::WouldBlock as i32,
        Ok(Completion::Ready(object)) if object == [9; 16] => 0,
        Ok(Completion::Ready(_)) => Error::Internal as i32,
        Err(error) => error as i32,
    }
}
