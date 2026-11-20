use playbit_api::{
    abi::data,
    context::{Completion, Context, OperationHandle, Principal, Provenance, ValueRef},
};
fn run(generation: u64, slot: u32) -> Option<()> {
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = Context::guest(&mut context_state).expect("valid events");
    let operation = OperationHandle::new(generation.try_into().ok()?, slot);
    let mut values = [data::Value::default(); 1];
    let mut bytes = [0; 3];
    let Completion::Ready(Some(result)) =
        context.take_authenticated_field(operation, &mut values, &mut bytes).ok()?
    else {
        return None;
    };
    if result.volume_id != [6; 16]
        || result.object_id != [9; 16]
        || result.type_id != [8; 16]
        || result.revision != [13; 32]
        || result.provenance
            != (Provenance::ApplicationEdit {
                principal: Principal::Human([11; 16]),
                device_id: [12; 16],
            })
        || !matches!(result.field.values()?.next()?, ValueRef::Text("abc"))
    {
        return None;
    }
    Some(())
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_authenticated(generation: u64, slot: u32) -> u32 {
    if run(generation, slot).is_some() { 0 } else { 1 }
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_authenticated_begin(generation: u64, slot: u32) -> u64 {
    let Ok(generation) = u32::try_from(generation) else { return 0 };
    Context::guest(&mut playbit_api::context::ContextState::default())
        .expect("valid events")
        .0
        .begin_read_authenticated_field(
            playbit_api::context::EntryHandle {
                volume: data::VolumeHandle::new(generation, slot).0,
                entry_id: [5; 15],
                ..Default::default()
            },
            [9; 16],
            [7; 16],
        )
        .map_or(0, |operation| operation.0)
}
