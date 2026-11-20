use playbit_api::{
    abi::{data::VolumeHandle, schema},
    context::{
        Completion, Context, OperationHandle, ReadError, SchemaField, SchemaType, ValueType,
    },
};
fn establish(generation: u64, slot: u32) -> Option<u64> {
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = Context::guest(&mut context_state).expect("valid events");
    let volume = VolumeHandle::new(generation.try_into().ok()?, slot);
    let entry = playbit_api::context::EntryHandle { volume: volume.0, entry_id: [5; 15], ..Default::default() };
    let fields =
        [SchemaField::new([7; 16], "link", ValueType::Reference([8; 16]), true, true).ok()?];
    let types = [SchemaType::new([8; 16], "Item", &fields, true).ok()?];
    Some(context.begin_establish_schema(entry, &types).ok()?.0)
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_schema_establish_begin(generation: u64, slot: u32) -> u64 {
    establish(generation, slot).unwrap_or(0)
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_schema_establish_take(generation: u64, slot: u32) -> i32 {
    let Ok(generation) = generation.try_into() else { return -3 };
    match Context::guest(&mut playbit_api::context::ContextState::default())
        .expect("valid events")
        .0
        .take_establish_schema(OperationHandle::new(generation, slot))
    {
        Ok(Completion::Ready(())) => 0,
        Ok(Completion::Pending) => -6,
        Err(error) => error as i32,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_schema_read_begin(generation: u64, slot: u32) -> u64 {
    let Ok(generation) = generation.try_into() else { return 0 };
    Context::guest(&mut playbit_api::context::ContextState::default())
        .expect("valid events")
        .0
        .begin_read_schema(playbit_api::context::EntryHandle {
            volume: VolumeHandle::new(generation, slot).0,
            entry_id: [5; 15],
            ..Default::default()
        })
        .map(|op| op.0)
        .unwrap_or(0)
}
fn read(generation: u64, slot: u32) -> Option<()> {
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = Context::guest(&mut context_state).expect("valid events");
    let operation = OperationHandle::new(generation.try_into().ok()?, slot);
    if !matches!(
        context.take_read_schema(operation, &mut [], &mut [], &mut []),
        Err(ReadError::BufferTooSmall { records: 1, values: 1, bytes: 8 })
    ) {
        return None;
    }
    let mut types = [schema::Type::default(); 1];
    let mut fields = [schema::Field::default(); 1];
    let mut bytes = [0; 8];
    let Completion::Ready(result) =
        context.take_read_schema(operation, &mut types, &mut fields, &mut bytes).ok()?
    else {
        return None;
    };
    let ty = result.types().next()?;
    let field = ty.fields().next()?;
    if ty.name != "Item"
        || !ty.deprecated
        || field.name != "link"
        || !field.deprecated
        || !field.repeated
        || field.value_type != ValueType::Reference([8; 16])
    {
        return None;
    }
    Some(())
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_schema(generation: u64, slot: u32) -> u32 {
    if read(generation, slot).is_some() { 0 } else { 1 }
}
