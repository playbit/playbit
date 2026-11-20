use playbit_api::{
    abi::data::VolumeHandle,
    context::{Completion, Context, FileReference},
};
fn run(generation: u64, slot: u32) -> Option<()> {
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = Context::guest(&mut context_state).expect("valid events");
    let volume = VolumeHandle::new(generation.try_into().ok()?, slot);
    let entry = playbit_api::context::EntryHandle { volume: volume.0, entry_id: [5; 15], ..Default::default() };
    let creation = context.begin_create_object(entry, [8; 16], &[]).ok()?;
    if context.poll_create_object(creation).ok()? != Completion::Ready([9; 16]) {
        return None;
    }
    let source = FileReference { volume: volume.0, entry_id: [9; 15], ..Default::default() };
    let destination = FileReference { volume: volume.0, entry_id: [10; 15], ..Default::default() };
    let copy = context.begin_file_copy(source, destination).ok()?;
    if context.poll_file_copy(copy).ok()? != Completion::Ready([10; 15]) {
        return None;
    }
    let selection = context.begin_file_selection().ok()?;
    if context.poll_file_selection(selection).ok()?
        != Completion::Ready(Some(FileReference { volume: volume.0, entry_id: [5; 15], ..Default::default() }))
    {
        return None;
    }
    Some(())
}
#[unsafe(no_mangle)]
pub extern "C" fn probe_pending(generation: u64, slot: u32) -> u32 {
    if run(generation, slot).is_some() { 0 } else { 1 }
}
