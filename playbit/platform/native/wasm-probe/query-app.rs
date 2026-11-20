use playbit_api::{
    abi::data,
    context::{Completion, Context, OperationHandle, PageStorage, Query, QueryError, ValueRef},
};
fn run(generation: u64, slot: u32) -> Option<()> {
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = Context::guest(&mut context_state).expect("valid events");
    let operation = OperationHandle::new(generation.try_into().ok()?, slot);
    let mut objects = [data::ObjectInfo::default(); 2];
    let mut fields = [data::Field::default(); 2];
    let mut values = [data::Value::default(); 2];
    let mut bytes = [0; 3];
    let Completion::Ready(page) = context
        .take_query_objects(
            operation,
            PageStorage {
                objects: &mut objects,
                fields: &mut fields,
                values: &mut values,
                bytes: &mut bytes,
            },
        )
        .ok()?
    else {
        return None;
    };
    if page.next != Some([10; 16]) || page.iter().len() != 2 {
        return None;
    }
    let mut objects = page.iter();
    let first = objects.next()?;
    let second = objects.next()?;
    if first.id != [9; 16]
        || second.id != [10; 16]
        || !matches!(first.record?.field([7; 16])?.values()?.next()?, ValueRef::Text("abc"))
        || !matches!(second.record?.field([7; 16])?.values()?.next()?, ValueRef::Boolean(true))
    {
        return None;
    }
    Some(())
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_query_begin(generation: u64, slot: u32) -> u64 {
    let Ok(generation) = generation.try_into() else { return 0 };
    let volume = data::VolumeHandle::new(generation, slot);
    let entry = playbit_api::context::EntryHandle { volume: volume.0, entry_id: [5; 15], ..Default::default() };
    let query = Query { type_id: [8; 16], after: None, limit: 2, fields: Some(&[[7; 16]]) };
    Context::guest(&mut playbit_api::context::ContextState::default())
        .expect("valid events")
        .0
        .begin_query_objects(entry, query)
        .map(|op| op.0)
        .unwrap_or(0)
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_query_take(generation: u64, slot: u32) -> i32 {
    let Ok(current) = generation.try_into() else { return -3 };
    let operation = OperationHandle::new(current, slot);
    match Context::guest(&mut playbit_api::context::ContextState::default())
        .expect("valid events")
        .0
        .take_query_objects(
            operation,
            PageStorage { objects: &mut [], fields: &mut [], values: &mut [], bytes: &mut [] },
        ) {
        Ok(Completion::Pending) => -6,
        Err(QueryError::Host(error)) => error as i32,
        Err(QueryError::BufferTooSmall { objects: 2, fields: 2, values: 2, bytes: 3 }) => {
            if run(generation, slot).is_some() { 0 } else { -4 }
        }
        _ => -4,
    }
}
